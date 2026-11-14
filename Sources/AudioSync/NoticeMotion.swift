import SwiftUI

/// A notice or the Dismiss All link under them, as one slot of the main window's `NoticeRoom`.
enum NoticeSlot: Hashable {
    case notice(String)
    case dismissAll
}

/// The main window's `NoticeRoom`, with the closing notices kept to draw until they have shut.
/// An object rather than @State, for the Command Line Tools toolchain.
final class NoticeRoomState: ObservableObject {
    @Published private(set) var room: NoticeRoom<NoticeSlot>
    private(set) var kept: [String: Notice]

    init(_ notices: [Notice]) {
        room = NoticeRoom(Self.slots(notices))
        kept = Dictionary(notices.map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
    }

    func follow(_ notices: [Notice], motion: NoticeMotion) {
        for notice in notices { kept[notice.id] = notice }
        room.follow(Self.slots(notices), at: Date(), duration: motion.duration)
        // Pauses the timeline once the move is over and lets a shut notice go.
        DispatchQueue.main.asyncAfter(deadline: .now() + motion.duration + 0.02) { [weak self] in
            guard let self else { return }
            self.room.prune(at: Date(), duration: motion.duration)
            let listed = Set(self.room.slots.compactMap { slot -> String? in
                if case .notice(let id) = slot.id { return id }
                return nil
            })
            self.kept = self.kept.filter { listed.contains($0.key) }
            self.objectWillChange.send()
        }
    }

    private static func slots(_ notices: [Notice]) -> [NoticeSlot] {
        notices.map { .notice($0.id) } + (notices.count > 1 ? [.dismissAll] : [])
    }
}

/// A slot's room and fade at `progress`, laid out every frame, so the AppKit scroll views
/// below follow it; clipped to the room it has. A closing one is already gone for clicks and VoiceOver.
struct NoticeReveal: ViewModifier {
    let progress: Double
    let motion: NoticeMotion
    let closing: Bool

    func body(content: Content) -> some View {
        PaneHeight(fraction: motion.room(at: progress)) { content.opacity(motion.opacity(at: progress, closing: closing)) }
            .clipped()
            .allowsHitTesting(!closing)
            .accessibilityHidden(closing)
    }
}
