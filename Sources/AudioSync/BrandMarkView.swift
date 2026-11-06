import SwiftUI

/// What the brand mark in the main window shows: it breathes while no
/// network speaker has turned up, rests once one has, and moves with the
/// input level while AudioSync plays to a speaker.
enum BrandMarkMood: Equatable {
    case searching, settled, playing

    static func of(running: Bool, noSpeakerOn: Bool, speakersFound: Bool) -> BrandMarkMood {
        if running && !noSpeakerOn { return .playing }
        return speakersFound ? .settled : .searching
    }
}

extension AppModel {
    var markMood: BrandMarkMood {
        .of(running: running, noSpeakerOn: noSpeakerOn,
            speakersFound: !layout.connected.isEmpty || hidden.inConnected)
    }

    var markFill: Color { running ? Brand.logo : Color.secondary }
}

/// The brand mark drawn as a decorative illustration, hidden from VoiceOver.
/// With Reduce Motion on it stays still.
struct BrandMarkView: View {
    let mood: BrandMarkMood
    let fill: Color
    @ObservedObject var level: InputLevel
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private static let breathPeriod = 4.5
    private static let breathScale: CGFloat = 0.04
    private static let pulseScale: CGFloat = 0.06

    var body: some View {
        // The clock runs only while the mark breathes and the window is on screen.
        let breathing = mood == .searching && !reduceMotion && level.visible
        TimelineView(.animation(minimumInterval: 1.0 / 30, paused: !breathing)) { context in
            let breath = breathing ? Self.breath(at: context.date) : 0
            BrandMark()
                .fill(fill)
                .scaleEffect(1 + Self.breathScale * breath + pulse)
                .opacity(1 - 0.2 * breath)
        }
        .animation(reduceMotion ? nil : .spring(duration: 0.7, bounce: 0.35), value: mood)
        .animation(reduceMotion ? nil : .easeOut(duration: 0.12), value: level.level)
        .accessibilityHidden(true)
        .allowsHitTesting(false)
    }

    private var pulse: CGFloat {
        guard mood == .playing, !reduceMotion else { return 0 }
        return Self.pulseScale * CGFloat(min(max(level.level, 0), 1))
    }

    /// 0 to 1 and back once per period, from a shared clock so every mark on screen breathes together.
    private static func breath(at date: Date) -> CGFloat {
        let phase = date.timeIntervalSinceReferenceDate / breathPeriod * 2 * .pi
        return CGFloat((1 - cos(phase)) / 2)
    }
}
