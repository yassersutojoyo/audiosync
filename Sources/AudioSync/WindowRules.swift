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

    /// `closingPress` is when the press on the icon that last closed the
    /// panel came, if one did; `latestPress` is when the latest press came,
    /// on the same clock (system uptime).
    /// Whether a press came while the panel still fades out (`closeEndsAt`,
    /// system uptime): it is ignored, as Outlook ignores one, rather than
    /// reopening the panel once the close is over.
    static func pressDuringClose(_ press: TimeInterval, _ closeEndsAt: TimeInterval?) -> Bool {
        closeEndsAt.map { press < $0 } ?? false
    }

    static func action(panelVisible: Bool, closingPress: TimeInterval?, latestPress: TimeInterval) -> StatusItemClick {
        if panelVisible { return .close }
        if let closingPress, abs(latestPress - closingPress) < samePress { return .stayClosed }
        return .open
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
/// With Reduce Motion it only fades. Only the panel's layer scales; the
/// window keeps its final size, so nothing is laid out again per frame.
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

    /// How far a fade has come `elapsed` seconds into it, on the ease-out
    /// curve Core Animation uses (control points 0, 0 and 0.58, 1).
    static func easedOut(_ elapsed: TimeInterval, over duration: TimeInterval) -> Double {
        guard duration > 0, elapsed < duration else { return 1 }
        let target = max(0, elapsed / duration)
        var low = 0.0, high = 1.0
        for _ in 0..<30 {
            let t = (low + high) / 2
            let x = 3 * (1 - t) * t * t * 0.58 + t * t * t
            if x < target { low = t } else { high = t }
        }
        let t = (low + high) / 2
        return 3 * (1 - t) * t * t + t * t * t
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
/// panel to finish growing in. The latest size wins.
struct PanelResize: Equatable {
    static let gather: TimeInterval = 0.1
    static let duration: TimeInterval = 0.2

    private(set) var pending: CGSize?
    private(set) var due: TimeInterval?

    /// When to apply the step this change joins. `settled` is when the
    /// panel's open animation ends.
    mutating func changed(to size: CGSize, at now: TimeInterval, settled: TimeInterval) -> TimeInterval {
        pending = size
        let when = due ?? max(now + Self.gather, settled)
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
    /// data, data that doesn't decode, or a band count other than the
    /// equalizer's (from a build with different bands).
    static func decode(_ data: Data?) -> Equalizer.Settings {
        guard let data, let saved = try? JSONDecoder().decode(Equalizer.Settings.self, from: data),
              saved.gains.count == Equalizer.frequencies.count,
              saved.preamp.isFinite, saved.gains.allSatisfy(\.isFinite)
        else { return Equalizer.Settings() }
        return saved
    }

    static func encode(_ settings: Equalizer.Settings) -> Data? {
        try? JSONEncoder().encode(settings)
    }
}
