import Foundation
import CoreGraphics

// Small decisions behind the windows and the model the views render, kept
// free of AppKit so the logic tests can check them.

/// Whether a saved window frame can still be reached.
enum WindowPlacement {
    /// How much of a window must lie on one screen's visible area for the
    /// user to grab its title bar and pull it back.
    static let minimumVisible = CGSize(width: 120, height: 60)

    /// True when `frame` overlaps one of `screens` (their visible areas) by at
    /// least `minimumVisible`. A frame saved on a display that is gone, or
    /// pushed under the menu bar, fails and the window is centered instead.
    static func isReachable(_ frame: CGRect, screens: [CGRect]) -> Bool {
        screens.contains { screen in
            let overlap = screen.intersection(frame)
            return !overlap.isNull
                && overlap.width >= minimumVisible.width && overlap.height >= minimumVisible.height
        }
    }
}

/// How the main window's width splits between its two columns. The speaker
/// list widens first, to its full width, then the left column, from
/// `narrow` up to `wide`; past that the speaker list again.
enum MainColumns {
    /// Where the master volume slider's track ended in the 280 pt left
    /// column: 209.5 pt, measured on docs/images/main-window-light.png.
    static let narrow: CGFloat = 210
    static let wide: CGFloat = 280
    /// The speaker list's width in the widest window, 860 pt.
    static let speakerListFull: CGFloat = 524

    /// The left column's width in a window `width` points wide: the speaker list widens
    /// first, to its full width, then this column.
    static func left(window width: CGFloat) -> CGFloat {
        let room = width - 2 * BrandTokens.Spacing.windowMargin - BrandTokens.Spacing.columnGap
        return min(max(room - speakerListFull, narrow), wide)
    }

    /// An estimate, not measured: wider than the widest transport symbol, the 22 pt Play.
    static let transportButton: CGFloat = 26

    /// The gap between the Now Playing buttons, `modes` while Shuffle or
    /// Repeat shows.
    static func transportSpacing(modes: Bool) -> CGFloat { modes ? 12 : 28 }

    /// The space inside the hero, around the source and the Now Playing buttons.
    static let heroPadding: CGFloat = 14
    /// The space between the left column's edge and a row's icon, which its hover fill covers.
    static let rowInset: CGFloat = 8
}

/// The main window's motion: one ease-out for rows that move or open, a
/// fully damped spring for controls that show on hover, and a plain fade
/// instead of either under Reduce Motion.
enum MainMotion {
    static let rowMove: TimeInterval = 0.35
    static let control: TimeInterval = 0.44
    static let reducedFade: TimeInterval = 0.15
    /// The window's spring when the zoom button widens or narrows it.
    static let zoom: TimeInterval = 0.56
}

/// Where the zoom button takes the main window: from any narrower width
/// out to the widest, `MainColumns`' 860 pt, and from there back to the slim
/// 696 pt. The top edge and the height stay; a wider window moves left as
/// far as it must to stay on `visible`.
enum MainZoom {
    static let slim: CGFloat = 696
    static let wide: CGFloat = 860

    static func target(_ frame: CGRect, visible: CGRect) -> CGRect {
        var out = frame
        out.size.width = frame.width < wide - 0.5 ? wide : slim
        if out.maxX > visible.maxX { out.origin.x = visible.maxX - out.width }
        out.origin.x = max(out.origin.x, visible.minX)
        return out
    }
}

/// How much the Now Playing art moves, `env`, 0 when still. It follows the
/// meter level's rises above its own recent average, never the loudness, so
/// a long flat note calms it and silence stills it about 12 s later.
struct Tide: Equatable {
    static let onset: TimeInterval = 0.35
    static let rise: TimeInterval = 0.45
    static let fall: TimeInterval = 1.6
    static let tail: TimeInterval = 3
    static let average: TimeInterval = 1.4
    static let stopAt = 0.02
    /// Drive per second of a level 1.0 above its average.
    static let gain = 40.0

    private(set) var env = 0.0
    private var drive = 0.0
    private var mean = 0.0

    /// `level` is the meter's 0-to-1 reading, nil when nothing is captured.
    mutating func step(level: Double?, dt: TimeInterval) {
        let now = level ?? 0
        drive = min(1.2, drive * exp(-dt / Self.onset) + max(0, now - mean) * Self.gain * dt)
        mean += (now - mean) * (1 - exp(-dt / Self.average))
        let playing = now > Self.stopAt
        let tau = drive > env ? Self.rise : (playing ? Self.fall : Self.tail)
        env += (drive - env) * (1 - exp(-dt / tau))
        if env < (playing ? 0.004 : Self.stopAt) { env = 0 }
    }
}

/// A title or name too long for its line scrolls: it rests at its start,
/// moves to its end at `speed`, rests there, moves back, and repeats.
enum Marquee {
    /// Points a second.
    static let speed: CGFloat = 30
    static let pause: TimeInterval = 2

    /// How far the title runs past its line; 0 when it fits, the line isn't
    /// laid out yet, or Reduce Motion is on, which cuts it with an ellipsis.
    static func overflow(text: CGFloat, line: CGFloat, reduceMotion: Bool) -> CGFloat {
        !reduceMotion && line > 0 && text > line ? text - line : 0
    }

    /// How far the title has moved left `elapsed` seconds after it showed.
    static func offset(elapsed: TimeInterval, overflow: CGFloat) -> CGFloat {
        let travel = TimeInterval(overflow / speed)
        let at = elapsed.truncatingRemainder(dividingBy: 2 * (pause + travel))
        if at < pause { return 0 }
        if at < pause + travel { return CGFloat(at - pause) * speed }
        if at < 2 * pause + travel { return overflow }
        return overflow - CGFloat(at - 2 * pause - travel) * speed
    }
}

/// How a notice comes and goes in the main window.
enum NoticeMotion: Equatable {
    /// Its height opens from the top as it fades in, the columns below moving
    /// down with it, and closes the same way.
    case grow
    /// With Reduce Motion: a cross-fade, the columns moving at once.
    case fade

    static func of(reduceMotion: Bool) -> NoticeMotion { reduceMotion ? .fade : .grow }

    /// Seconds.
    var duration: TimeInterval { self == .grow ? 0.24 : 0.15 }

    /// Seconds a closing notice fades before its room starts to close, so the
    /// columns don't slide up under it while it still shows.
    var closeDelay: TimeInterval { self == .grow ? 0.09 : 0 }

    /// Seconds a notice takes to fade in or out.
    var fade: TimeInterval { self == .grow ? 0.18 : duration }
}

/// What a click on the menu bar icon does to its panel. The press on the icon
/// closes the popover before the click itself arrives, and AppKit may drop
/// that click's action or still deliver it. A click whose press is the one
/// that closed the panel leaves it closed; any later press opens it.
enum StatusItemClick: Equatable {
    case open
    case close
    /// The click that already closed the panel: leave it closed.
    case stayClosed

    /// Two press times this close are the same press, read twice; two real
    /// clicks are far further apart.
    static let samePress: TimeInterval = 0.02

    /// How recent a mouse press must be for the panel closing now to be its doing.
    static let pressRecency: TimeInterval = 0.5

    /// Whether the panel closing now was caused by a press on the icon: a
    /// press came just now with the pointer on `itemFrame`. AppKit's y grows
    /// upward, and the top edge counts: a pointer pushed against the top of
    /// the screen sits exactly on it.
    static func pressClosed(pointer: CGPoint, itemFrame: CGRect, secondsSincePress: TimeInterval) -> Bool {
        secondsSincePress >= 0 && secondsSincePress < pressRecency
            && pointer.x >= itemFrame.minX && pointer.x < itemFrame.maxX
            && pointer.y > itemFrame.minY && pointer.y <= itemFrame.maxY
    }

    /// Whether a press came while the panel still fades out (`closeEndsAt`,
    /// system uptime): it is ignored, as Outlook ignores one, rather than
    /// reopening the panel once the close is over.
    static func pressDuringClose(_ press: TimeInterval, _ closeEndsAt: TimeInterval?) -> Bool {
        closeEndsAt.map { press < $0 } ?? false
    }

    /// `closingPress` is when the press on the icon that last closed the
    /// panel came, if one did; `latestPress` is when the latest press came,
    /// on the same clock (system uptime).
    static func action(panelVisible: Bool, closingPress: TimeInterval?, latestPress: TimeInterval) -> StatusItemClick {
        if panelVisible { return .close }
        if let closingPress, abs(latestPress - closingPress) < samePress { return .stayClosed }
        return .open
    }
}

/// What the menu bar panel does when its popover asks to close, as a
/// transient popover does on a click outside or when its window loses key.
/// With the panel's own motion the popover never closes itself: an open
/// panel starts its own close instead, and one that already fades out
/// (until `closeEndsAt`, system uptime) stays until that fade closes it.
enum PopoverCloseRequest: Equatable {
    case allow
    case closeOurWay
    case wait

    static func answer(systemAnimation: Bool, presented: Bool, now: TimeInterval,
                       closeEndsAt: TimeInterval?) -> PopoverCloseRequest {
        if systemAnimation { return .allow }
        if presented { return .closeOurWay }
        return StatusItemClick.pressDuringClose(now, closeEndsAt) ? .wait : .allow
    }
}

/// When the menu bar panel's popover follows its content's size. NSPopover
/// animates a size change on top of its own open or close animation, which
/// stutters, so a change waits until the popover is fully shown and is
/// dropped once it starts to close.
enum PanelSizing: Equatable {
    case closed, opening, shown, closing

    enum Event { case show, didShow, willClose, didClose }

    var followsContent: Bool { self == .shown }

    /// One `show()`; true when the open finishes now. AppKit sends no didShow
    /// to a popover still on screen, as one reopened while its close fade
    /// runs, so the show stands in for it.
    mutating func show(onScreen: Bool) -> Bool {
        self = after(.show)
        return onScreen
    }

    /// Ends the open once; false when it already ended or the panel moved on.
    mutating func finishOpen() -> Bool {
        guard self == .opening else { return false }
        self = after(.didShow)
        return true
    }

    func after(_ event: Event) -> PanelSizing {
        switch (self, event) {
        case (_, .show): return .opening
        case (.opening, .didShow): return .shown
        case (.opening, .willClose), (.shown, .willClose): return .closing
        case (.closing, .didClose): return .closed
        default: return self   // a late notice from an earlier open or close
        }
    }
}

/// The menu bar panel's height outside its speaker list (header, source,
/// volume, footer, margins), plus room for what may show around it.
enum PanelChrome {
    static func height(notices: Int, permissionCard: Bool = false, airPlayNote: Bool = false,
                       castAddRow: Bool = false, presets: Bool = false, liveInputHint: Bool = false,
                       noSpeakerOn: Bool = false) -> CGFloat {
        var height: CGFloat = 280   // the hero, with a song and its art, and the 36 pt This Mac row
        // The compact panel shows the newest notice, with a footer line when others wait.
        if notices > 0 { height += 90 }
        if notices > 1 { height += 30 }
        if permissionCard { height += 140 }
        if airPlayNote { height += 70 }
        if castAddRow { height += 20 }
        if presets { height += 38 }
        if liveInputHint { height += 48 }
        if noSpeakerOn { height += 32 }
        return height
    }
}

/// The panel's Add by Address row. It is offered once the Cast search found
/// nothing, and its room is kept while the search still runs: the search
/// ends on a timer, often with the panel open, which must not grow then.
enum CastAddRow: Equatable {
    case none, reserved, shown

    /// How long the row takes to fade into the room kept for it.
    static let fadeIn: TimeInterval = 0.15

    static func state(canAdd: Bool, list: CastListState) -> CastAddRow {
        guard canAdd else { return .none }
        switch list {
        case .noneFound, .failed: return .shown
        case .searching: return .reserved
        case .quiet: return .none
        }
    }
}

/// How the menu bar panel opens and closes: it grows out of the menu bar icon
/// on a spring too damped to bounce, and shrinks back quickly as it fades.
/// With Reduce Motion it only fades. Both step the popover window's own frame
/// (`PanelReveal`) around content laid out once at its final size.
struct PanelMotion: Equatable {
    /// Where an open starts, or where a close ends; 1 is a plain fade.
    var scale: Double
    /// The fade, and for a close the shrink too.
    var duration: TimeInterval
    /// A spring for the open's growth; none when 0.
    var stiffness = 0.0
    var damping = 0.0

    static let mass = 1.0

    static func open(reduceMotion: Bool) -> PanelMotion {
        PanelMotion(scale: reduceMotion ? 1 : 0.92, duration: 0.18, stiffness: 441, damping: 38.64)
    }

    /// What is left of the growth when the spring animation ends: the tail
    /// cut there moves the panel's far edge by under half a point.
    static let tail = 0.01

    static func close(reduceMotion: Bool) -> PanelMotion {
        PanelMotion(scale: reduceMotion ? 1 : 0.96, duration: 0.14)
    }

    var grows: Bool { scale != 1 }

    /// 1 is critical damping; just under it the overshoot stays far below a point.
    var dampingRatio: Double { stiffness > 0 ? damping / (2 * (stiffness * Self.mass).squareRoot()) : 1 }

    /// The fraction of the growth the spring overshoots by.
    var overshoot: Double {
        let ratio = dampingRatio
        return ratio < 1 ? exp(-Double.pi * ratio / (1 - ratio * ratio).squareRoot()) : 0
    }

    /// When the growth stays within `tail` of its end; the fade alone without a spring.
    var settles: TimeInterval {
        guard stiffness > 0, grows else { return duration }
        let ratio = min(dampingRatio, 0.999)
        let decay = ratio * (stiffness / Self.mass).squareRoot()
        return max(duration, (-log(Self.tail) - log((1 - ratio * ratio).squareRoot())) / decay)
    }
}

/// How the panel's rows move when one comes, goes or changes height: one
/// ease-out, without a bounce. With Reduce Motion they take their places at once.
enum PanelRowMotion {
    static let duration: TimeInterval = 0.35
    /// The gap between rows that come or go together, and between the
    /// panel's parts as it opens.
    static let stagger: TimeInterval = 0.035
}

/// How the panel's parts fade in and rise as it opens (`PanelIntro`), each a
/// stagger after the one above it.
enum PanelIntroMotion {
    static let duration: TimeInterval = 0.44
    static let lastPart = 6

    static func delay(_ index: Int) -> TimeInterval { 0.09 + Double(index) * PanelRowMotion.stagger }

    static var end: TimeInterval { delay(lastPart) + duration }
}

/// When the open panel takes a new size from its content, on the rows' own
/// curve. A grow starts at once, so the panel opens room as a row comes in; a
/// shrink waits until the rows have settled, gathering the changes that come
/// meanwhile into one step. Both wait for the panel to finish opening. The
/// latest size wins.
struct PanelResize: Equatable {
    static let gather = PanelRowMotion.duration
    static let duration = PanelRowMotion.duration

    private(set) var pending: CGSize?
    private(set) var due: TimeInterval?

    /// When to apply the step this change joins. `settled` is when the
    /// panel's open animation ends; `current` is the content size on screen.
    mutating func changed(to size: CGSize, at now: TimeInterval, settled: TimeInterval,
                          from current: CGSize? = nil) -> TimeInterval {
        pending = size
        // Grown content runs off the bottom edge until its step starts, so a grow never waits out the gather.
        let grows = current.map { size.height > $0.height } ?? false
        let when = min(due ?? .infinity, max(now + (grows ? 0 : Self.gather), settled))
        due = when
        return when
    }

    /// The size to animate to, once its step is due.
    mutating func take(at now: TimeInterval) -> CGSize? {
        guard let due, now >= due - 0.001, let size = pending else { return nil }
        pending = nil
        self.due = nil
        return size
    }

    /// The popover's window frame for content of `size`, its top edge (under
    /// the icon) and its middle where they are. The popover itself would set
    /// it in one frame.
    static func windowFrame(_ frame: CGRect, content: CGSize, to size: CGSize) -> CGRect {
        let dw = size.width - content.width, dh = size.height - content.height
        return CGRect(x: frame.minX - dw / 2, y: frame.minY - dh, width: frame.width + dw, height: frame.height + dh)
    }
}

/// The open panel's window frame for each content size, worked out from the
/// frame the popover itself gave it once the open was over. A resize that
/// starts while another runs aims from there too, never from the half-grown
/// window, so the top edge under the icon never drifts.
struct PanelRest: Equatable {
    private(set) var frame: CGRect
    private(set) var content: CGSize
    /// The menu bar icon's middle (screen x) when `frame` was set; nil if unknown.
    private(set) var icon: CGFloat?

    init(frame: CGRect, content: CGSize, icon: CGFloat? = nil) {
        self.frame = frame
        self.content = content
        self.icon = icon
    }

    /// Where the next resize aims from: the window as it is when no step
    /// runs, as AppKit may have moved the open panel since; else the rest.
    static func next(_ rest: PanelRest?, stepRunning: Bool, window: CGRect, content: CGSize,
                     icon: CGFloat? = nil) -> PanelRest {
        guard stepRunning, let rest else { return PanelRest(frame: window, content: content, icon: icon) }
        return rest
    }

    /// The frame for content of `size`, with the icon's middle now at `now`,
    /// which becomes the new rest.
    mutating func move(to size: CGSize, icon now: CGFloat? = nil) -> CGRect {
        var aim = PanelResize.windowFrame(frame, content: content, to: size)
        // Once it learns the size the popover re-centers the window on the icon, as far as a screen edge lets it.
        if let then = icon, let now {
            let off = frame.midX - then
            let mid = abs(off) <= 1 ? aim.midX + now - then : off < 0 ? min(now, aim.midX) : max(now, aim.midX)
            aim.origin.x += mid - aim.midX
        }
        frame = aim
        content = size
        icon = now ?? icon
        return frame
    }
}

/// The steps of the popover window's frame as the panel opens or closes,
/// each with its edges on the pixel grid.
enum PanelReveal {
    /// The time per step: a frame at 120 Hz, which a 60 Hz display shows every other one of.
    static let step: TimeInterval = 1.0 / 120

    /// When the open's last step has had its frame.
    static func duration(_ motion: PanelMotion) -> TimeInterval { Double(scales(motion, step: step).count) * step }

    /// `body`, the panel's window frame, scaled by `scale` about the arrow's
    /// tip, its top and bottom rounded to the nearest `pixel`, its middle and
    /// each half of its width moved by whole points; with no `pixel`, unrounded.
    static func body(_ body: CGRect, scale: Double, about tip: CGPoint, pixel: CGFloat) -> CGRect {
        let s = CGFloat(scale)
        func snap(_ value: CGFloat) -> CGFloat { pixel > 0 ? (value / pixel).rounded() * pixel : value }
        func scaled(_ value: CGFloat, _ origin: CGFloat) -> CGFloat { snap(origin + (value - origin) * s) }
        // The system draws its arrow at the window's middle and keeps the window on whole points, so a
        // half-point side edge put the arrow half a point off the tip on every odd width.
        let middle = body.midX - ((body.midX - tip.x) * (1 - s)).rounded()
        let half = body.width / 2 - (body.width / 2 * (1 - s)).rounded()
        let minX = pixel > 0 ? middle - half : scaled(body.minX, tip.x)
        let width = pixel > 0 ? 2 * half : scaled(body.maxX, tip.x) - minX
        let minY = scaled(body.minY, tip.y), maxY = scaled(body.maxY, tip.y)
        return CGRect(x: minX, y: minY, width: width, height: maxY - minY)
    }

    /// Where `content`, laid out in the window at its `rest` frame, goes in the
    /// window at `step`, so it stays put on screen and the window clips it.
    static func content(_ content: CGRect, rest: CGRect, step: CGRect) -> CGRect {
        content.offsetBy(dx: rest.minX - step.minX, dy: rest.minY - step.minY)
    }

    /// The arrow's tip on screen, which the window and its content scale
    /// about: under the icon's middle `icon`, on the top edge of the window at
    /// `rest`, and inside it, so it stays on the panel's own screen.
    static func tip(icon: CGFloat, rest: CGRect) -> CGPoint {
        // Whole points, so the window's pixel-snapped steps never move the content's anchor.
        CGPoint(x: min(max(icon, rest.minX), rest.maxX).rounded(), y: rest.maxY.rounded())
    }

    /// The transform for a layer that draws the content `scale` times its size
    /// with the point laid at `anchor` drawn at `tip`; Core Animation applies
    /// it about the layer's position, `offset` from `anchor`. All in the
    /// layer's superlayer's coordinates, where the transform acts.
    static func contentScale(_ scale: Double, tip: CGPoint, anchor: CGPoint, offset: CGVector) -> CGAffineTransform {
        let s = CGFloat(scale)
        return CGAffineTransform(a: s, b: 0, c: 0, d: s, tx: tip.x - anchor.x - (1 - s) * offset.dx,
                                 ty: tip.y - anchor.y - (1 - s) * offset.dy)
    }

    /// The open's spring, one scale per `step` from `motion.scale` to exactly 1
    /// at `motion.settles`, matching a `CASpringAnimation` with no initial velocity.
    static func scales(_ motion: PanelMotion, step: TimeInterval) -> [Double] {
        guard motion.grows, motion.stiffness > 0, step > 0 else { return [motion.scale, 1] }
        let natural = (motion.stiffness / PanelMotion.mass).squareRoot()
        let ratio = min(motion.dampingRatio, 0.999)
        let damped = natural * (1 - ratio * ratio).squareRoot()
        let count = Int((motion.settles / step).rounded(.up))
        return (0...count).map { index in
            guard index > 0 else { return motion.scale }
            guard index < count else { return 1 }
            let t = Double(index) * step
            let left = exp(-ratio * natural * t) * (cos(damped * t) + ratio * natural / damped * sin(damped * t))
            return 1 - (1 - motion.scale) * left
        }
    }

    /// The close's shrink from `start` to `motion.scale`, one scale per `step`
    /// over `motion.duration`, on Core Animation's ease-in curve (0.42, 0, 1, 1).
    static func shrinks(from start: Double, _ motion: PanelMotion, step: TimeInterval) -> [Double] {
        guard motion.grows, motion.duration > 0, step > 0 else { return [start, motion.scale] }
        let count = Int((motion.duration / step).rounded(.up))
        return (0...count).map { index in
            guard index > 0 else { return start }
            guard index < count else { return motion.scale }
            let time = Double(index) * step / motion.duration
            // The curve gives time and progress as functions of one parameter; find it for this time.
            var low = 0.0, high = 1.0
            for _ in 0..<40 {
                let u = (low + high) / 2
                let v = 1 - u
                if (1.26 * v * v + (3 * v + u) * u) * u < time { low = u } else { high = u }
            }
            let u = (low + high) / 2
            return start + (motion.scale - start) * (3 * (1 - u) + u) * u * u
        }
    }
}

/// The arrow the panel draws over the popover's own, which the system edges
/// in its bare material, so the arrow reads in the panel's backing like the box.
enum PanelArrow {
    /// 13 pt high, the popover's top safe area inset; 2 pt wider than a 28 pt base, to cover its soft edge.
    static let size = CGSize(width: 30, height: 13)
    /// What the arrow's base keeps clear of the window's side edges for its rounded corners;
    /// an estimate, since AppKit does not publish the popover's corner radius.
    static let cornerRoom: CGFloat = 10

    /// Where the arrow goes on screen for the popover window at `window`:
    /// apex on the window's top edge at the arrow's `tip`, where the system
    /// draws its own, kept clear of the window's corners.
    static func frame(in window: CGRect, tip: CGPoint) -> CGRect {
        let inset = size.width / 2 + cornerRoom
        let apex = min(max(tip.x, window.minX + inset), window.maxX - inset)
        return CGRect(x: apex - size.width / 2, y: window.maxY - size.height, width: size.width, height: size.height)
    }
}

/// How a zone folds or unfolds in the menu bar panel: the rows fade, the
/// arrow turns and the panel resizes as one motion that starts with the
/// click, on one ease-in-out curve. With Reduce Motion it all changes at once.
struct PanelFold: Equatable {
    static let duration: TimeInterval = 0.2

    /// How long the panel's resize and the rows' movement take; 0 is at once.
    var duration: TimeInterval

    init(reduceMotion: Bool) { duration = reduceMotion ? 0 : Self.duration }

    var moves: Bool { duration > 0 }

    /// What a zone shows of itself: while a fold moves, only its own moving
    /// area, so the rows below never slide over its fading rows; at rest
    /// room to spare, so focus rings aren't cut.
    static func clip(_ rect: CGRect, moving: Bool) -> CGRect {
        moving ? rect : rect.insetBy(dx: -40, dy: -40)
    }
}

/// Which answers of a background read of macOS's state still hold when they
/// reach the main thread: only the newest read's, and its login item status
/// only if Launch at Login wasn't switched while it ran.
struct ExternalReads: Equatable {
    struct Ticket: Equatable {
        let read: Int, loginChanges: Int
    }

    private(set) var reads = 0
    private(set) var loginChanges = 0

    mutating func begin() -> Ticket {
        reads += 1
        return Ticket(read: reads, loginChanges: loginChanges)
    }

    mutating func loginChanged() { loginChanges += 1 }

    func applies(_ ticket: Ticket) -> Bool { true }

    func loginApplies(_ ticket: Ticket) -> Bool { true }
}

/// When work that follows a menu bar panel open or close may run: once its
/// animation is over. Main-thread work that lands inside it drops frames.
struct PanelQuiet: Equatable {
    enum Animation { case open, close }

    /// The popover's own grow and fade (`system`), from the click to the end
    /// of its bounce and of its fade as measured on the Mac; else the
    /// panel's own motion (`PanelMotion`), an open's until its last step has shown (`PanelReveal`)
    /// and, unless Reduce Motion skips them (`intro`), its parts have faded in (`PanelIntroMotion`).
    static func duration(_ animation: Animation, system: Bool, intro: Bool = true) -> TimeInterval {
        switch (animation, system) {
        case (.open, true): return 0.6
        case (.close, true): return 0.25
        case (.open, false): return max(PanelReveal.duration(.open(reduceMotion: false)), intro ? PanelIntroMotion.end : 0)
        case (.close, false): return PanelMotion.close(reduceMotion: false).duration
        }
    }

    var system = false

    /// System uptime when the last animation ends.
    private(set) var busyUntil: TimeInterval = 0

    init(system: Bool = false) { self.system = system }

    mutating func began(_ animation: Animation, at now: TimeInterval, intro: Bool = true) {
        busyUntil = max(busyUntil, now + Self.duration(animation, system: system, intro: intro))
    }

    func wait(at now: TimeInterval) -> TimeInterval { max(0, busyUntil - now) }
}

/// Work waiting for `PanelQuiet`, at most one per label.
struct QuietQueue: Equatable {
    var quiet = PanelQuiet()
    private(set) var waiting = Set<String>()

    enum Step: Equatable {
        case wait(TimeInterval)
        case run
    }

    /// False when work under `label` already waits: the new request is dropped.
    mutating func enqueue(_ label: String) -> Bool { waiting.insert(label).inserted }

    /// Waiting work under `label`, checked at `now`: it waits on while an
    /// open or close animates (one may have begun since it last checked),
    /// else it runs and leaves the queue.
    mutating func step(_ label: String, at now: TimeInterval) -> Step {
        let wait = quiet.wait(at: now)
        if wait > 0 { return .wait(wait) }
        waiting.remove(label)
        return .run
    }
}

/// How strongly a scrolling column fades at its top and bottom edges, from 0
/// (nothing hidden past that edge) to 1 (at least the fade's height hidden).
struct ScrollEdgeFades: Equatable {
    var top: CGFloat
    var bottom: CGFloat

    static let none = ScrollEdgeFades(top: 0, bottom: 0)

    /// `visibleTop` and `visibleHeight` give the visible part in content
    /// coordinates; `fade` is the height over which an edge reaches full strength.
    static func of(contentHeight: CGFloat, visibleTop: CGFloat, visibleHeight: CGFloat,
                   fade: CGFloat) -> ScrollEdgeFades {
        guard fade > 0 else { return .none }
        func strength(_ hidden: CGFloat) -> CGFloat { min(max(hidden / fade, 0), 1) }
        return ScrollEdgeFades(top: strength(visibleTop),
                               bottom: strength(contentHeight - visibleTop - visibleHeight))
    }
}

/// The entries of the source picker, in the order it lists them.
enum SourceList {
    struct Entry: Equatable {
        /// `id` is `AudioSource.defaultsValue`: "" for the whole system, the
        /// bundle ID for an app, "input:<uid>" for an input device.
        let id, name: String
        let isInput: Bool
        /// False only for the chosen input device while it is unplugged.
        var connected = true
        /// False for the chosen input device while it is unplugged or a hands-free input,
        /// and in the several-apps menu for a checked app that isn't running.
        var available = true
    }

    /// The whole system first, then the apps by name, then the input devices
    /// by name, the UID breaking ties. The chosen app stays listed while it isn't
    /// running (its name comes from `chosenAppName`), so the picker still
    /// says what will be captured; so does a chosen input device that is
    /// unplugged, listed last as `unpluggedInputName`, not connected.
    /// Devices without a UID can't be chosen and are skipped, and a Bluetooth
    /// headset's hands-free input (`AudioDevices.isHandsFreeInput`) is left
    /// out; a chosen one is listed last under its name, not available. While
    /// no mix is chosen, `lastMix` (S354) is listed where a chosen mix would be.
    static func entries(systemName: String,
                        apps: [(bundleID: String, name: String)],
                        inputs: [Input],
                        chosen: AudioSource,
                        chosenAppName: (String) -> String,
                        mixName: (AppMixMode, [String]) -> String = { $1.joined(separator: ", ") },
                        unpluggedInputName: String = "",
                        lastMix: AudioSource? = nil) -> [Entry] {
        var mixEntry: [Entry] = []
        var listedMix = lastMix
        if case .mix = chosen { listedMix = chosen }
        if let listedMix, case .mix(let mode, let ids) = listedMix {
            let names = ids.map { id in apps.first { $0.bundleID == id }?.name ?? chosenAppName(id) }
            mixEntry = [Entry(id: listedMix.defaultsValue, name: mixName(mode, names), isInput: false)]
        }
        var appEntries = apps.map { Entry(id: $0.bundleID, name: $0.name, isInput: false) }
        if case .app(let bundleID) = chosen, !appEntries.contains(where: { $0.id == bundleID }) {
            appEntries.append(Entry(id: bundleID, name: chosenAppName(bundleID), isInput: false))
            appEntries.sort { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
        }
        var inputEntries = inputs.filter { leftOut($0) == nil }.map {
            Entry(id: AudioSource.input(uid: $0.uid).defaultsValue, name: $0.name, isInput: true)
        }.sorted {
            let order = $0.name.localizedCaseInsensitiveCompare($1.name)
            return order == .orderedSame ? $0.id < $1.id : order == .orderedAscending
        }
        if case .input(let uid) = chosen {
            let plugged = inputs.first { $0.uid == uid }
            if plugged.map({ leftOut($0) != nil }) ?? true {
                inputEntries.append(Entry(id: chosen.defaultsValue, name: plugged?.name ?? unpluggedInputName,
                                          isInput: true, connected: plugged != nil, available: false))
            }
        }
        return [Entry(id: AudioSource.system.defaultsValue, name: systemName, isInput: false)]
            + mixEntry + appEntries + inputEntries
    }

    /// The apps the several-apps menu offers: the running apps, plus the
    /// checked ones and, while no mix is chosen, `lastMix`'s (S354) that
    /// aren't running, so each can still be unchecked or picked again. By
    /// name, as the picker sorts.
    static func mixApps(_ apps: [(bundleID: String, name: String)], checked: [String],
                        chosenAppName: (String) -> String, chosen: AudioSource = .system,
                        lastMix: AudioSource? = nil) -> [Entry] {
        var entries = apps.map { Entry(id: $0.bundleID, name: $0.name, isInput: false) }
        var listedMix = lastMix
        if case .mix = chosen { listedMix = chosen }
        var kept: [String] = []
        if let listedMix, case .mix(_, let ids) = listedMix { kept = ids }
        for id in checked + kept where !entries.contains(where: { $0.id == id }) {
            entries.append(Entry(id: id, name: chosenAppName(id), isInput: false, available: false))
        }
        return entries.sorted { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
    }

    /// An input device as Core Audio lists it; `transport` is
    /// `kAudioDeviceTransportType…`, 0 when the device doesn't say, and
    /// `inputSubTransports` an aggregate's (`AudioDevices.Device`).
    struct Input {
        let uid, name: String
        let transport: UInt32
        var inputSubTransports: [UInt32] = []
    }

    /// Why the picker leaves an input out, for the debug log; nil when it lists it.
    static func leftOut(_ input: Input) -> String? {
        if input.uid.isEmpty { return "no UID" }
        if AudioDevices.isHandsFreeInput(transport: input.transport, inputSubTransports: input.inputSubTransports) {
            return "a Bluetooth hands-free input, which would switch the headset to call mode"
        }
        return nil
    }

    /// One debug-log line per input device: its transport and whether the
    /// picker lists it.
    static func logLines(_ inputs: [Input]) -> [String] {
        inputs.map { input in
            let verdict = leftOut(input).map { "left out: \($0)" } ?? "listed"
            let subs = input.inputSubTransports.isEmpty ? ""
                : " (sub-device inputs \(input.inputSubTransports.map(AudioDevices.transportCode).joined(separator: ", ")))"
            return "[AudioSync] source picker: input \"\(input.name)\" (\(input.uid)), "
                + "transport \(AudioDevices.transportCode(input.transport))\(subs): \(verdict)"
        }
    }
}

/// Closing a PIN or password prompt that nothing waits for any more
/// (`NativeAirPlayModel.withdrawPrompt`). The app can close it only while
/// it is the frontmost modal window, or it would close another alert in its
/// place. With another modal over it (an alert opened from its run loop),
/// the close stays pending and is tried again until the prompt is frontmost
/// or gone, so it never stays open for a PIN that goes nowhere. The model
/// has already let go of the prompt, so whatever it returns is no answer.
struct PromptWithdrawal {
    enum Step: Equatable {
        /// Close the prompt now.
        case close
        /// Another modal is on top: try again shortly.
        case wait
        /// Nothing to do.
        case none
    }

    /// A withdrawn prompt is still open under another modal window.
    private(set) var pending = false

    /// The open prompt was taken back. `open`: the prompt still runs;
    /// `frontmost`: it is the frontmost modal window. `.wait` starts the
    /// retries; while they run, a second withdrawal starts none.
    mutating func withdraw(open: Bool, frontmost: Bool) -> Step {
        guard open, !pending else { return .none }
        return step(frontmost: frontmost)
    }

    /// A retry while the close is pending.
    mutating func retry(open: Bool, frontmost: Bool) -> Step {
        guard pending else { return .none }
        guard open else { pending = false; return .none }
        return step(frontmost: frontmost)
    }

    /// The prompt returned (closed or answered): a close still pending
    /// would otherwise hit the next prompt.
    mutating func ended() { pending = false }

    private mutating func step(frontmost: Bool) -> Step {
        pending = !frontmost
        return frontmost ? .close : .wait
    }
}

/// Where a modal prompt or alert starts its modal loop from. A loop
/// started inside a main-queue block holds back every later main-queue
/// block (the sender's events among them) until it ends.
enum PromptStart {
    static func later(_ body: @escaping () -> Void) {
        RunLoop.main.perform(inModes: [.common], block: body)
        CFRunLoopWakeUp(CFRunLoopGetMain())   // perform alone waits for the loop's next wake
    }
}

/// Equalizer settings as the app stores them (JSON in UserDefaults).
enum StoredEqualizer {
    /// The saved settings, or the defaults when no data or no JSON object is
    /// saved. A value that doesn't read costs only itself, as on Windows: the
    /// switch is off, the preamp or a band 0 dB. A band count other than the
    /// equalizer's (from a build with different bands) keeps the saved bands,
    /// flat for missing ones, extras dropped.
    static func decode(_ data: Data?) -> Equalizer.Settings {
        guard let data, let saved = try? JSONDecoder().decode(Lenient.self, from: data)
        else { return Equalizer.Settings() }
        let count = Equalizer.frequencies.count
        let gains = (saved.gains ?? []).map { $0.db ?? .nan }
        var stepped = Equalizer.Settings()
        stepped.enabled = saved.enabled ?? false
        stepped.preamp = Equalizer.quantized(saved.preamp ?? 0)
        stepped.gains = (Array(gains.prefix(count)) + [Double](repeating: 0, count: max(0, count - gains.count)))
            .map(Equalizer.quantized)
        return stepped
    }

    private struct Lenient: Decodable {
        enum Key: String, CodingKey { case enabled, preamp, gains }
        struct Gain: Decodable {
            let db: Double?
            init(from decoder: Decoder) throws { db = try? decoder.singleValueContainer().decode(Double.self) }
        }
        let enabled: Bool?, preamp: Double?, gains: [Gain]?
        init(from decoder: Decoder) throws {
            let values = try decoder.container(keyedBy: Key.self)
            enabled = try? values.decode(Bool.self, forKey: .enabled)
            preamp = try? values.decode(Double.self, forKey: .preamp)
            gains = try? values.decode([Gain].self, forKey: .gains)
        }
    }

    static func encode(_ settings: Equalizer.Settings) -> Data? {
        try? JSONEncoder().encode(settings)
    }
}
