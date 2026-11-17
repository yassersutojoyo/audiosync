// Renders AudioSync's app screenshots from mock data: the main window, the menu
// bar panel and the equalizer, in light and dark appearance.
//
// Built and run by scripts/screenshots/render.sh, together with every app source
// except main.swift. The views are the app's own (MainView, MenuBarView,
// EqualizerView), hosted in offscreen windows and drawn with cacheDisplay, so
// AppKit-backed controls (sliders, menus, switches) look as they do in the app.
// SwiftUI's ImageRenderer can't draw those. Nothing appears on screen, and the
// app's settings, engine and devices are never touched.
//
// Usage: render <output-dir> <Poppins-Medium.ttf> [--review | --layar]
// --layar writes only the LAYAR states, each as a PNG and a JSON element list.

import AppKit
import SwiftUI

let arguments = CommandLine.arguments
guard arguments.count >= 3 else {
    FileHandle.standardError.write(Data("usage: render <output-dir> <Poppins-Medium.ttf> [--review | --layar]\n".utf8))
    exit(2)
}
let outputDirectory = URL(fileURLWithPath: arguments[1], isDirectory: true)
let reviewStates = arguments.dropFirst(3).contains("--review")

/// Pixels per point in every image, whatever displays are attached.
let scale: CGFloat = 2

// The app bundles Poppins Medium; a bare binary has to register it to draw the wordmark.
var fontError: Unmanaged<CFError>?
guard CTFontManagerRegisterFontsForURL(URL(fileURLWithPath: arguments[2]) as CFURL, .process, &fontError) else {
    fatalError("Could not register \(arguments[2]): \(String(describing: fontError?.takeRetainedValue()))")
}

// No Dock icon, no menu bar, never takes focus.
NSApplication.shared.setActivationPolicy(.prohibited)

// MARK: - Mock data

/// Generic names only: never real device, network or account names.
enum Mock {
    static func model(_ configure: (AppModel) -> Void = { _ in }) -> AppModel {
        let model = AppModel()
        let text = model.text
        model.running = true
        model.sources = [AppModel.Source(id: "", name: text.sourceSystem, icon: nil, isInput: false)]
        model.master = 72
        model.nowPlaying = NowPlayingMonitor.Track(
            bundleID: "com.example.player", appName: "Player", title: "Midnight Drive",
            artist: "Example Artist", album: "Example Album", isPlaying: true, artwork: artwork())
        model.equalizerOn = true
        let curve = Mock.equalizer()
        model.equalizerCurve = text.equalizerCurveName(curve.shownCurve, saved: curve.curves)
        model.zones = [AppModel.Zone(id: "zone:1", name: "Downstairs", volume: 60, members: [
            AppModel.ZoneMember(key: SpeakerKey.make(id: "living", kind: .airplay), name: "Living Room", symbol: "hifispeaker", speakerID: "living"),
            AppModel.ZoneMember(key: SpeakerKey.make(id: "kitchen", kind: .airplay), name: "Kitchen", symbol: "hifispeaker", speakerID: "kitchen"),
        ], on: 2, present: 2, allOn: true)]
        model.airplay = [
            AppModel.Speaker(id: "living", name: "Living Room", tag: .airplay, enabled: true, volume: 64, offsetMs: nil),
            AppModel.Speaker(id: "kitchen", name: "Kitchen", tag: .airplay, enabled: true, volume: 48, offsetMs: nil,
                             autoConnect: true),
            AppModel.Speaker(id: "bedroom", name: "Bedroom", tag: .airplay, enabled: false, volume: 35, offsetMs: nil,
                             availability: .needsCode),
            // The sender plays one AirPlay 2 speaker at a time, so only this one is on.
            AppModel.Speaker(id: "study", name: "Study", tag: .airplay, enabled: true, volume: 52, offsetMs: nil,
                             airPlay2: true),
        ]
        // Offsets are relative to the built-in speakers, the reference, which has none.
        model.local = [
            AppModel.Speaker(id: "built-in", name: "MacBook Pro Speakers", tag: .builtIn, enabled: true, volume: 60,
                             offsetMs: nil, isReference: true),
            AppModel.Speaker(id: "headphones", name: "Studio Headphones", tag: .bluetooth, enabled: true, volume: 40,
                             offsetMs: -170, calibrationLeftOut: .headphones),
            AppModel.Speaker(id: "desk", name: "Desk Speakers", tag: .usb, enabled: false, volume: 55, offsetMs: 50),
        ]
        model.cast = [
            AppModel.Speaker(id: CastID.app("living-tv"), name: "Living Room TV", tag: .cast, enabled: true,
                             volume: 50, offsetMs: 120,
                             cast: CastRow(status: .playing, muted: false, volumeFixed: false, group: nil)),
        ]
        model.presets = [
            PresetRow(id: "preset:1", name: "Evening", current: true, picked: true),
            PresetRow(id: "preset:2", name: "Kitchen Only"),
        ]
        model.toggleDelayPanel("headphones")
        configure(model)
        return model
    }

    /// Nothing streaming, a notice waiting, no network speaker found yet while
    /// the Google Cast search runs, and the local speakers chosen last time
    /// waiting for streaming to start.
    static func idleModel() -> AppModel {
        model { model in
            model.running = false
            model.notices.post(model.text.sourceTerminated(name: "Player"), subject: Notice.source)
            model.nowPlaying = nil
            model.equalizerOn = false
            model.zones = []
            model.airplay = []
            model.cast = []
            model.castList = .searching
            model.closeDelayPanels()
        }
    }

    static func emptyModel() -> AppModel {
        model { model in
            model.running = false
            model.nowPlaying = nil
            model.zones = []
            model.airplay = []
            model.local = []
            model.cast = []
            model.castList = .noneFound
            model.closeDelayPanels()
        }
    }

    static func equalizer(enabled: Bool = true) -> EqualizerModel {
        var settings = Equalizer.Settings()
        settings.enabled = enabled
        settings.preamp = enabled ? -2 : 0
        if enabled, let loudness = Equalizer.presets.first(where: { $0.id == .loudness }) {
            settings.gains = loudness.gains
        }
        return EqualizerModel(settings: settings)
    }

    /// Placeholder album art: a plain gradient drawn here, no third-party artwork.
    static func artwork() -> NSImage {
        NSImage(size: NSSize(width: 112, height: 112), flipped: false) { rect in
            NSGradient(colors: [NSColor(srgbRed: 0.93, green: 0.48, blue: 0.36, alpha: 1),
                                NSColor(srgbRed: 0.42, green: 0.25, blue: 0.75, alpha: 1)])?
                .draw(in: rect, angle: -45)
            return true
        }
    }
}

// MARK: - Rendering

enum Look: CaseIterable {
    case light, dark

    var appearance: NSAppearance { NSAppearance(named: self == .dark ? .darkAqua : .aqua)! }
    var suffix: String { self == .dark ? "dark" : "light" }
    /// Stands in for the menu bar panel's popover material, which needs a real
    /// desktop behind it.
    var panelFill: NSColor { self == .dark ? NSColor(white: 43 / 255, alpha: 1) : NSColor(white: 246 / 255, alpha: 1) }
    var outline: NSColor { self == .dark ? NSColor(white: 1, alpha: 0.16) : NSColor(white: 0, alpha: 0.16) }
    var shadow: NSColor { NSColor(white: 0, alpha: self == .dark ? 0.5 : 0.22) }
}

/// Draws like the focused window of the active app, as a window looks in use:
/// accent-colored switches and sliders, colored window buttons. Offscreen
/// windows otherwise draw their controls grey, like an inactive window.
/// `hasKeyAppearance` is AppKit's own, undocumented check. This dev-only renderer
/// overrides it; the app never does. If a macOS release drops it, the shots
/// come out in the inactive look instead of failing.
final class ShotWindow: NSWindow {
    @objc(hasKeyAppearance) func drawsAsKey() -> Bool { true }
}

/// Lets SwiftUI finish its pending updates (layout, onAppear) before drawing.
func settle() {
    RunLoop.main.run(until: Date().addingTimeInterval(0.3))
}

func blankImage(_ size: NSSize) -> NSBitmapImageRep {
    guard let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: Int((size.width * scale).rounded()),
        pixelsHigh: Int((size.height * scale).rounded()), bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true,
        isPlanar: false, colorSpaceName: .calibratedRGB, bytesPerRow: 0, bitsPerPixel: 0)?
        .retagging(with: .sRGB) else { fatalError("Could not allocate a \(size) bitmap") }
    rep.size = size
    return rep
}

/// The view as drawn in its window, in sRGB at `scale`.
func capture(_ view: NSView) -> NSBitmapImageRep {
    view.layoutSubtreeIfNeeded()
    settle()
    let rep = blankImage(view.bounds.size)
    view.cacheDisplay(in: view.bounds, to: rep)
    return rep
}

/// Rounds the corners of a shot, puts it on a transparent canvas with a window
/// shadow, and optionally fills behind it and outlines it.
func framed(_ shot: NSBitmapImageRep, cornerRadius: CGFloat, look: Look,
            fill: NSColor? = nil, outline: Bool = true) -> NSBitmapImageRep {
    let margin: CGFloat = 24
    let canvas = blankImage(NSSize(width: shot.size.width + 2 * margin, height: shot.size.height + 2 * margin))
    NSGraphicsContext.saveGraphicsState()
    defer { NSGraphicsContext.restoreGraphicsState() }
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: canvas)
    let rect = NSRect(origin: NSPoint(x: margin, y: margin), size: shot.size)
    let shape = NSBezierPath(roundedRect: rect, xRadius: cornerRadius, yRadius: cornerRadius)

    NSGraphicsContext.saveGraphicsState()
    let shadow = NSShadow()   // offset and blur are in pixels, not points
    shadow.shadowColor = look.shadow
    shadow.shadowOffset = NSSize(width: 0, height: -6 * scale)
    shadow.shadowBlurRadius = 18 * scale
    shadow.set()
    (fill ?? .black).setFill()
    shape.fill()
    NSGraphicsContext.restoreGraphicsState()

    NSGraphicsContext.saveGraphicsState()
    shape.addClip()
    if let fill {
        fill.setFill()
        shape.fill()
    }
    shot.draw(in: rect, from: .zero, operation: .sourceOver, fraction: 1, respectFlipped: false, hints: nil)
    NSGraphicsContext.restoreGraphicsState()

    if outline {
        let edge = NSBezierPath(roundedRect: rect.insetBy(dx: 0.25, dy: 0.25),
                                xRadius: cornerRadius - 0.25, yRadius: cornerRadius - 0.25)
        edge.lineWidth = 0.5
        look.outline.setStroke()
        edge.stroke()
    }
    return canvas
}

func write(_ rep: NSBitmapImageRep, _ name: String) {
    let url = outputDirectory.appendingPathComponent(name)
    guard let png = rep.representation(using: .png, properties: [:]) else { fatalError("Could not encode \(name)") }
    do { try png.write(to: url) } catch { fatalError("Could not write \(url.path): \(error)") }
    print("  \(name)  \(rep.pixelsWide)×\(rep.pixelsHigh)")
}

/// Corner radius of a titled window on current macOS.
let windowCornerRadius: CGFloat = 16

/// How far the tallest scroll view under `view` would have to scroll; 0 when
/// none has to.
func scrollOverflow(in view: NSView) -> CGFloat {
    if let scroll = view as? NSScrollView, let document = scroll.documentView {
        return max(0, document.frame.height - scroll.contentView.bounds.height)
    }
    return view.subviews.map(scrollOverflow).max() ?? 0
}

/// The main window as MainWindowController sets it up, grown from its usual
/// size until the mock speakers fit without scrolling.
func mainWindowShot(_ model: AppModel, look: Look) -> NSBitmapImageRep {
    let host = NSHostingController(rootView: MainView(model: model))
    host.sizingOptions = [.minSize]
    var shotSize = NSSize(width: 860, height: 610)
    let chrome: NSWindow.StyleMask = [.titled, .resizable, .closable, .miniaturizable]
    let window = ShotWindow(contentRect: NSRect(origin: .zero, size: shotSize),
                            styleMask: chrome, backing: .buffered, defer: false)
    window.appearance = look.appearance
    window.contentViewController = host
    window.titleVisibility = .hidden
    window.titlebarAppearsTransparent = true
    window.setContentSize(shotSize)
    window.contentView?.layoutSubtreeIfNeeded()
    settle()
    if let content = window.contentView {
        let overflow = scrollOverflow(in: content).rounded(.up)
        if overflow > 0 {
            shotSize.height += overflow
            window.setContentSize(shotSize)
        }
    }
    // The frame view draws the title bar and its window buttons too.
    guard let frameView = window.contentView?.superview else { fatalError("The window has no frame view") }
    return framed(capture(frameView), cornerRadius: windowCornerRadius, look: look)
}

/// The equalizer window as EqualizerWindowController sets it up.
func equalizerShot(_ model: EqualizerModel, look: Look) -> NSBitmapImageRep {
    let window = ShotWindow(contentViewController: NSHostingController(rootView: EqualizerView(model: model, curveName: CurveNameDraft(), curvesFull: {}, askRename: { _ in })))
    window.appearance = look.appearance
    window.title = UIStrings().equalizerTitle
    window.styleMask = [.titled, .closable]
    guard let frameView = window.contentView?.superview else { fatalError("The window has no frame view") }
    return framed(capture(frameView), cornerRadius: windowCornerRadius, look: look)
}

/// The menu bar panel's content, on a flat stand-in for its material.
func panelShot(_ model: AppModel, look: Look) -> NSBitmapImageRep {
    let host = NSHostingView(rootView: MenuBarView(model: model))
    host.frame = NSRect(origin: .zero, size: host.fittingSize)
    let window = ShotWindow(contentRect: host.frame, styleMask: [.borderless], backing: .buffered, defer: false)
    window.appearance = look.appearance
    window.contentView = host
    // In the app the popover draws the edge; here the frame does.
    return framed(capture(host), cornerRadius: BrandTokens.Radius.panel, look: look, fill: look.panelFill)
}

// MARK: - LAYAR states

/// The commonest pixel in `rect` (points from the top left) as the background,
/// and the commonest one whose channels differ from it by more than 96 in sum
/// as the foreground, one sample per point: the desktop's layar_shots samples
/// its photos the same way (`sampled_colours`).
func sampledColours(_ rep: NSBitmapImageRep, _ rect: CGRect) -> (String?, String?) {
    guard let data = rep.bitmapData else { return (nil, nil) }
    let perPoint = CGFloat(rep.pixelsWide) / rep.size.width
    var counts: [UInt32: Int] = [:]
    for y in stride(from: max(0, Int(rect.minY)), to: min(Int(rep.size.height), Int(rect.maxY)), by: 1) {
        for x in stride(from: max(0, Int(rect.minX)), to: min(Int(rep.size.width), Int(rect.maxX)), by: 1) {
            let at = Int((CGFloat(y) + 0.5) * perPoint) * rep.bytesPerRow
                + Int((CGFloat(x) + 0.5) * perPoint) * (rep.bitsPerPixel / 8)
            counts[UInt32(data[at]) << 16 | UInt32(data[at + 1]) << 8 | UInt32(data[at + 2]), default: 0] += 1
        }
    }
    func commonest(_ keep: (UInt32) -> Bool) -> UInt32? {
        counts.filter { keep($0.key) }.max { ($0.value, $0.key) < ($1.value, $1.key) }?.key
    }
    guard let bg = commonest({ _ in true }) else { return (nil, nil) }
    func channels(_ rgb: UInt32) -> [Int] { [Int(rgb >> 16 & 255), Int(rgb >> 8 & 255), Int(rgb & 255)] }
    let far = { (rgb: UInt32) in zip(channels(rgb), channels(bg)).map { abs($0 - $1) }.reduce(0, +) > 96 }
    let hex = { (rgb: UInt32) in String(format: "#%06x", rgb) }
    return (commonest(far).map(hex), hex(bg))
}

/// Every accessibility element under `view` with a label, title or text value,
/// as scripts/screen_compare.py reads it. The accessibility tree carries no
/// font, so `font` is null.
func layarElements(in view: NSView, rep: NSBitmapImageRep) -> [[String: Any]] {
    var found: [[String: Any]] = []
    let page = CGRect(origin: .zero, size: view.bounds.size)
    func attribute(_ node: NSObject, _ name: String) -> Any? {
        node.accessibilityAttributeValue(NSAccessibility.Attribute(rawValue: name))
    }
    func walk(_ node: NSObject, depth: Int) {
        guard depth < 64 else { return }
        let text = ["AXDescription", "AXTitle", "AXValue"].lazy
            .compactMap { attribute(node, $0) as? String }
            .first { !$0.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }
        if let text, let element = node as? NSAccessibilityElementProtocol, let window = view.window {
            var rect = view.convert(window.convertFromScreen(element.accessibilityFrame()), from: nil)
            if !view.isFlipped { rect.origin.y = view.bounds.height - rect.maxY }
            if rect.width > 0, rect.height > 0, rect.intersects(page) {
                let (fg, bg) = sampledColours(rep, rect)
                found.append(["key": text,
                              "frame": [rect.minX, rect.minY, rect.width, rect.height].map { Double($0) },
                              "font": NSNull(), "fg": (fg as Any?) ?? NSNull(), "bg": (bg as Any?) ?? NSNull()])
            }
        }
        for case let child as NSObject in (attribute(node, "AXChildren") as? [Any]) ?? [] {
            walk(child, depth: depth + 1)
        }
    }
    walk(view, depth: 0)
    return found
}

/// The main window's content as MainWindowController sets it up
/// (MainWindow.swift:22-38), at its default 696 x 600 points.
func layarMain(_ model: AppModel, look: Look) -> (NSWindow, NSView) {
    let host = NSHostingController(rootView: MainView(model: model))
    host.sizingOptions = [.minSize]
    let window = ShotWindow(contentRect: NSRect(x: 0, y: 0, width: 696, height: 600),
                            styleMask: [.titled, .resizable, .closable, .miniaturizable],
                            backing: .buffered, defer: false)
    window.appearance = look.appearance
    window.backgroundColor = look == .dark ? .windowBackgroundColor : .white
    window.contentViewController = host
    window.titleVisibility = .hidden
    window.titlebarAppearsTransparent = true
    window.setContentSize(NSSize(width: 696, height: 600))
    return (window, host.view)
}

func layarPanel(_ model: AppModel, look: Look) -> (NSWindow, NSView) {
    let host = NSHostingView(rootView: MenuBarView(model: model))
    host.frame = NSRect(origin: .zero, size: host.fittingSize)
    let window = ShotWindow(contentRect: host.frame, styleMask: [.borderless], backing: .buffered, defer: false)
    window.appearance = look.appearance
    window.backgroundColor = look.panelFill
    window.contentView = host
    return (window, host)
}

func layarEqualizer(look: Look) -> (NSWindow, NSView) {
    let window = ShotWindow(contentViewController: NSHostingController(rootView: EqualizerView(
        model: Mock.equalizer(), curveName: CurveNameDraft(), curvesFull: {}, askRename: { _ in })))
    window.appearance = look.appearance
    window.styleMask = [.titled, .closable]
    guard let content = window.contentView else { fatalError("The equalizer window has no content view") }
    return (window, content)
}

/// The track the desktop's layar_shots plays, so the card's texts match by key.
func layarTrack(playing: Bool = true, long: Bool = false) -> NowPlayingMonitor.Track {
    NowPlayingMonitor.Track(
        bundleID: "com.example.player", appName: "Player",
        title: long ? "A track title long enough to need more than the two lines the card shows" : "A track title",
        artist: long ? "An artist name long enough to run past the end of its line" : "Artist",
        album: "Example Album", isPlaying: playing, artwork: Mock.artwork())
}

/// Mock.model with no delay panel open and the desktop's track playing.
func layarModel(_ configure: (AppModel) -> Void = { _ in }) -> AppModel {
    Mock.model { model in
        model.closeDelayPanels()
        model.nowPlaying = layarTrack()
        configure(model)
    }
}

func layarIdle(_ model: AppModel) {
    model.running = false
    model.nowPlaying = nil
}

func layarNoSpeakers(_ model: AppModel) {
    model.zones = []
    model.airplay = []
    model.local = []
    model.cast = []
    model.castList = .noneFound
    model.nowPlaying = nil
}

/// The desktop's LONG name on the Kitchen row, and the long track.
func layarLongNames(_ model: AppModel) {
    model.airplay[1] = AppModel.Speaker(
        id: "kitchen", name: "Living Room Speaker Next to the Big Window by the Bookshelves", tag: .airplay,
        enabled: true, volume: 48, offsetMs: nil, autoConnect: true)
    model.nowPlaying = layarTrack(long: true)
}

/// The LAYAR rows (notes IDs, shared with the desktop's layar_shots) that mock
/// data can set up here.
let layarRows: [(id: String, draw: (Look) -> (NSWindow, NSView))] = [
    ("MAIN-IDLE", { layarMain(layarModel(layarIdle), look: $0) }),
    ("MAIN-PLAYING", { layarMain(layarModel(), look: $0) }),
    ("MAIN-HEADER", { layarMain(layarModel(), look: $0) }),
    ("MAIN-HEADER-OFF", { layarMain(layarModel { $0.running = false }, look: $0) }),
    ("MAIN-VOLUME-ROWS", { layarMain(layarModel(), look: $0) }),
    ("MAIN-NOWPLAYING-PAUSED", { layarMain(layarModel { $0.nowPlaying = layarTrack(playing: false) }, look: $0) }),
    ("MAIN-EMPTY", { layarMain(layarModel(layarNoSpeakers), look: $0) }),
    ("MAIN-LONGNAME", { layarMain(layarModel(layarLongNames), look: $0) }),
    ("MAIN-DELAY-OPEN", { layarMain(layarModel { $0.toggleDelayPanel("headphones") }, look: $0) }),
    ("TRAY-IDLE", { layarPanel(layarModel(layarIdle), look: $0) }),
    ("TRAY-PLAYING", { layarPanel(layarModel(), look: $0) }),
    ("TRAY-EMPTY", { layarPanel(layarModel(layarNoSpeakers), look: $0) }),
    ("TRAY-LONGNAME", { layarPanel(layarModel(layarLongNames), look: $0) }),
    ("EQ-WINDOW", { layarEqualizer(look: $0) }),
]

/// The fewest keyed elements MAIN-PLAYING may list: its header, source,
/// Now Playing card, sidebar buttons and speaker rows come to well over this.
let layarPlayingFloor = 30

/// Writes `<ID>-terang|gelap.png` and `.json` per row, unframed, at `scale`.
func writeLayar() {
    var playingCount = 0
    for (id, draw) in layarRows {
        for (look, theme) in [(Look.light, "terang"), (Look.dark, "gelap")] {
            NSApp.appearance = look.appearance
            let (window, view) = draw(look)
            let rep = capture(view)
            write(rep, "\(id)-\(theme).png")
            let elements = layarElements(in: view, rep: rep)
            if id == "MAIN-PLAYING", look == .light { playingCount = elements.count }
            let listed: [String: Any] = [
                "window": [Double(view.bounds.width), Double(view.bounds.height)],
                "elements": elements,
            ]
            let url = outputDirectory.appendingPathComponent("\(id)-\(theme).json")
            do {
                try JSONSerialization.data(withJSONObject: listed, options: [.sortedKeys]).write(to: url)
            } catch { fatalError("Could not write \(url.path): \(error)") }
            withExtendedLifetime(window) {}
        }
    }
    guard playingCount >= layarPlayingFloor else {
        FileHandle.standardError.write(Data("MAIN-PLAYING lists \(playingCount) keyed elements, fewer than \(layarPlayingFloor)\n".utf8))
        exit(1)
    }
}

// MARK: - Output

try FileManager.default.createDirectory(at: outputDirectory, withIntermediateDirectories: true)
print("Rendering into \(outputDirectory.path)")
if arguments.dropFirst(3).contains("--layar") {
    writeLayar()
    exit(0)
}
for look in Look.allCases {
    NSApp.appearance = look.appearance
    write(mainWindowShot(Mock.model(), look: look), "main-window-\(look.suffix).png")
    write(panelShot(Mock.model(), look: look), "menu-bar-panel-\(look.suffix).png")
    write(equalizerShot(Mock.equalizer(), look: look), "equalizer-\(look.suffix).png")
    if reviewStates {
        write(mainWindowShot(Mock.idleModel(), look: look), "review-main-window-idle-\(look.suffix).png")
        write(mainWindowShot(Mock.emptyModel(), look: look), "review-main-window-empty-\(look.suffix).png")
        write(panelShot(Mock.idleModel(), look: look), "review-menu-bar-panel-idle-\(look.suffix).png")
        write(panelShot(Mock.emptyModel(), look: look), "review-menu-bar-panel-empty-\(look.suffix).png")
        write(equalizerShot(Mock.equalizer(enabled: false), look: look), "review-equalizer-off-\(look.suffix).png")
    }
}
