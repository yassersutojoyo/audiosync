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
/// With Reduce Motion it only fades. The open grows a mask over the panel
/// (`PanelReveal`) and the close scales its layer; the window keeps its
/// final size, so nothing is laid out again per frame.
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

    /// The shift that keeps `anchor` (measured from the layer's anchor point)
    /// in place while the layer scales by `scale`.
    static func pin(_ anchor: CGPoint, scale: Double) -> CGPoint {
        let rest = CGFloat(1 - scale)
        return CGPoint(x: anchor.x * rest, y: anchor.y * rest)
    }
}

/// When the open panel takes a new size from its content. Changes that come
/// within `gather` of the first go in one animated step, which waits for the
/// panel to finish growing in; content that grew starts it at once. The
/// latest size wins.
struct PanelResize: Equatable {
    static let gather: TimeInterval = 0.1
    static let duration: TimeInterval = 0.2

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

/// The panel grows in under a pixel-aligned mask, its content at its own size:
/// a scaled layer is resampled, so its hairlines blur until the scale ends.
enum PanelReveal {
    /// The panel's body scaled by `scale` about the arrow's tip, its edges
    /// rounded to the nearest `pixel`.
    static func body(_ body: CGRect, scale: Double, about tip: CGPoint, pixel: CGFloat) -> CGRect {
        let s = CGFloat(scale)
        func snap(_ value: CGFloat) -> CGFloat { pixel > 0 ? (value / pixel).rounded() * pixel : value }
        func scaled(_ value: CGFloat, _ origin: CGFloat) -> CGFloat { snap(origin + (value - origin) * s) }
        let minX = scaled(body.minX, tip.x), maxX = scaled(body.maxX, tip.x)
        let minY = scaled(body.minY, tip.y), maxY = scaled(body.maxY, tip.y)
        return CGRect(x: minX, y: minY, width: maxX - minX, height: maxY - minY)
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
}

/// The shadow AudioSync draws around the menu bar panel while it grows in or
/// shrinks away, so it scales with the panel: the system's own shadow keeps
/// the window's full size. It stands in for the system's only during the
/// motion, so its values aim at the system's; they are estimates until
/// compared with it on the Mac.
struct PanelShadow: Equatable {
    var radius: CGFloat
    var opacity: Double
    /// Up is positive, as in AppKit.
    var offsetY: CGFloat

    static func style(dark: Bool) -> PanelShadow {
        PanelShadow(radius: 14, opacity: dark ? 0.5 : 0.25, offsetY: -5)
    }

    /// Room around the window for the shadow to spread into.
    var margin: CGFloat { (radius * 2 + abs(offsetY)).rounded(.up) }

    static let cornerRadius: CGFloat = 10
    /// The arrow's base for its height.
    static let arrowWidthRatio: CGFloat = 2.2

    /// The panel's outline: its rounded body, and the arrow over it up to `tip`.
    static func outline(body: CGRect, tip: CGPoint) -> CGPath {
        let path = CGMutablePath()
        path.addRoundedRect(in: body, cornerWidth: cornerRadius, cornerHeight: cornerRadius)
        let height = tip.y - body.maxY
        guard height > 0 else { return path }
        let half = height * arrowWidthRatio / 2
        path.move(to: CGPoint(x: tip.x - half, y: body.maxY))
        path.addLine(to: tip)
        path.addLine(to: CGPoint(x: tip.x + half, y: body.maxY))
        path.closeSubpath()
        return path
    }

    /// Where the shadow may show, filled even-odd: inside `bounds` but
    /// outside the outline, since under the translucent panel it would darken it.
    static func outside(_ outline: CGPath, in bounds: CGRect) -> CGPath {
        let path = CGMutablePath()
        path.addRect(bounds)
        path.addPath(outline)
        return path
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

/// Which parts of the popover's own background the panel's stand-in hides
/// while the panel scales. A background view that holds the panel's content
/// (the popover's frame) stays, or the content and arrow would go with it;
/// of such a view only its material layers are hidden.
enum PanelBackdrop {
    static func hidden<Node>(in node: Node, children: (Node) -> [Node], isContent: (Node) -> Bool,
                             isStandIn: (Node) -> Bool, isBackground: (Node) -> Bool) -> [Node] {
        walk(node, children: children, isContent: isContent, isStandIn: isStandIn, isBackground: isBackground).hidden
    }

    /// The sublayers of a view's layer that no subview owns, apart from
    /// AppKit's focus ring: it belongs to the focused control, and AppKit
    /// hides and shows it on its own.
    static func material<Layer: AnyObject>(in sublayers: [Layer], keeping owned: [Layer],
                                           name: (Layer) -> String? = { _ in nil }) -> [Layer] {
        sublayers.filter { layer in
            !owned.contains { $0 === layer } && !(name(layer)?.contains("FocusRing") ?? false)
        }
    }

    private static func walk<Node>(_ node: Node, children: (Node) -> [Node], isContent: (Node) -> Bool,
                                   isStandIn: (Node) -> Bool,
                                   isBackground: (Node) -> Bool) -> (hidden: [Node], holdsContent: Bool) {
        if isContent(node) { return ([], true) }
        if isStandIn(node) { return ([], false) }
        var hidden: [Node] = []
        var holdsContent = false
        for child in children(node) {
            let below = walk(child, children: children, isContent: isContent, isStandIn: isStandIn,
                             isBackground: isBackground)
            hidden += below.hidden
            holdsContent = holdsContent || below.holdsContent
        }
        if isBackground(node) && !holdsContent { hidden.insert(node, at: 0) }
        return (hidden, holdsContent)
    }
}

/// When work that follows a menu bar panel open or close may run: once its
/// animation is over. Main-thread work that lands inside it drops frames.
struct PanelQuiet: Equatable {
    enum Animation { case open, close }

    /// The popover's own grow and fade (`system`), from the click to the end
    /// of its bounce and of its fade as measured on the Mac; else the
    /// panel's own motion (`PanelMotion`).
    static func duration(_ animation: Animation, system: Bool) -> TimeInterval {
        switch (animation, system) {
        case (.open, true): return 0.6
        case (.close, true): return 0.25
        case (.open, false): return PanelMotion.open(reduceMotion: false).settles
        case (.close, false): return PanelMotion.close(reduceMotion: false).duration
        }
    }

    var system = false

    /// System uptime when the last animation ends.
    private(set) var busyUntil: TimeInterval = 0

    init(system: Bool = false) { self.system = system }

    mutating func began(_ animation: Animation, at now: TimeInterval) {
        busyUntil = max(busyUntil, now + Self.duration(animation, system: system))
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
    }

    /// The whole system first, then the apps by name, then the input devices
    /// in Core Audio's order. The chosen app stays listed while it isn't
    /// running (its name comes from `chosenAppName`), so the picker still
    /// says what will be captured; a chosen input device that is unplugged
    /// is not listed. Devices without a UID can't be chosen and are skipped,
    /// and a Bluetooth headset's hands-free input
    /// (`AudioDevices.isHandsFreeInput`) is left out, chosen or not.
    static func entries(systemName: String,
                        apps: [(bundleID: String, name: String)],
                        inputs: [Input],
                        chosen: AudioSource,
                        chosenAppName: (String) -> String,
                        mixName: (AppMixMode, [String]) -> String = { $1.joined(separator: ", ") }) -> [Entry] {
        var mixEntry: [Entry] = []
        if case .mix(let mode, let ids) = chosen {
            let names = ids.map { id in apps.first { $0.bundleID == id }?.name ?? chosenAppName(id) }
            mixEntry = [Entry(id: chosen.defaultsValue, name: mixName(mode, names), isInput: false)]
        }
        var appEntries = apps.map { Entry(id: $0.bundleID, name: $0.name, isInput: false) }
        if case .app(let bundleID) = chosen, !appEntries.contains(where: { $0.id == bundleID }) {
            appEntries.append(Entry(id: bundleID, name: chosenAppName(bundleID), isInput: false))
            appEntries.sort { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
        }
        let inputEntries = inputs.filter { leftOut($0) == nil }.map {
            Entry(id: AudioSource.input(uid: $0.uid).defaultsValue, name: $0.name, isInput: true)
        }
        return [Entry(id: AudioSource.system.defaultsValue, name: systemName, isInput: false)]
            + mixEntry + appEntries + inputEntries
    }

    /// The apps the several-apps menu offers: the running apps, plus the
    /// checked ones that aren't running, so each can still be unchecked. By
    /// name, as the picker sorts.
    static func mixApps(_ apps: [(bundleID: String, name: String)], checked: [String],
                        chosenAppName: (String) -> String) -> [Entry] {
        var entries = apps.map { Entry(id: $0.bundleID, name: $0.name, isInput: false) }
        for id in checked where !entries.contains(where: { $0.id == id }) {
            entries.append(Entry(id: id, name: chosenAppName(id), isInput: false))
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

/// Equalizer settings as the app stores them (JSON in UserDefaults).
enum StoredEqualizer {
    /// The saved settings, or the defaults when nothing usable is saved: no
    /// data or data that doesn't decode. A band count other than the
    /// equalizer's (from a build with different bands) keeps the saved bands,
    /// flat for missing ones, extras dropped.
    static func decode(_ data: Data?) -> Equalizer.Settings {
        guard let data, let saved = try? JSONDecoder().decode(Equalizer.Settings.self, from: data),
              saved.preamp.isFinite, saved.gains.allSatisfy(\.isFinite)
        else { return Equalizer.Settings() }
        let count = Equalizer.frequencies.count
        var stepped = saved
        stepped.preamp = Equalizer.quantized(saved.preamp)
        stepped.gains = (Array(saved.gains.prefix(count)) + [Double](repeating: 0, count: max(0, count - saved.gains.count)))
            .map(Equalizer.quantized)
        return stepped
    }

    static func encode(_ settings: Equalizer.Settings) -> Data? {
        try? JSONEncoder().encode(settings)
    }
}
