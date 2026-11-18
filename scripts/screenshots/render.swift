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
/// and the one farthest from it (channel sum, more than 96) as the foreground,
/// one sample per point: the desktop's layar_shots samples
/// its photos the same way (`sampled_colours`). The capture leaves the window's
/// background out (transparent pixels read as black), so each pixel is laid
/// over `behind` (0-255 sRGB) first, as the window shows it.
func sampledColours(_ rep: NSBitmapImageRep, _ rect: CGRect, behind: [Int]) -> (String?, String?) {
    guard let data = rep.bitmapData else { return (nil, nil) }
    let perPoint = CGFloat(rep.pixelsWide) / rep.size.width
    var counts: [UInt32: Int] = [:]
    for y in stride(from: max(0, Int(rect.minY)), to: min(Int(rep.size.height), Int(rect.maxY)), by: 1) {
        for x in stride(from: max(0, Int(rect.minX)), to: min(Int(rep.size.width), Int(rect.maxX)), by: 1) {
            let at = Int((CGFloat(y) + 0.5) * perPoint) * rep.bytesPerRow
                + Int((CGFloat(x) + 0.5) * perPoint) * (rep.bitsPerPixel / 8)
            // The rep's samples are premultiplied by alpha.
            let clear = 255 - Int(data[at + 3])
            let rgb = (0..<3).map { UInt32(Int(data[at + $0]) + clear * behind[$0] / 255) }
            counts[rgb[0] << 16 | rgb[1] << 8 | rgb[2], default: 0] += 1
        }
    }
    func commonest(_ keep: (UInt32) -> Bool) -> UInt32? {
        counts.filter { keep($0.key) }.max { ($0.value, $0.key) < ($1.value, $1.key) }?.key
    }
    guard let bg = commonest({ _ in true }) else { return (nil, nil) }
    func channels(_ rgb: UInt32) -> [Int] { [Int(rgb >> 16 & 255), Int(rgb >> 8 & 255), Int(rgb & 255)] }
    let distance = { (rgb: UInt32) in zip(channels(rgb), channels(bg)).map { abs($0 - $1) }.reduce(0, +) }
    let farthest = counts.filter { distance($0.key) > 96 }
        .max { (distance($0.key), $0.value, $0.key) < (distance($1.key), $1.value, $1.key) }?.key
    let hex = { (rgb: UInt32) in String(format: "#%06x", rgb) }
    return (farthest.map(hex), hex(bg))
}

/// The `layarKey` marks of the view last drawn, in its points from the top left.
final class LayarSink {
    var marks: [(key: String, frame: CGRect)] = []
    func take(_ marks: [LayarMark], _ proxy: GeometryProxy) {
        self.marks = marks.map { ($0.key, proxy[$0.anchor]) }
    }
}

let layarSink = LayarSink()

extension View {
    /// Hands the marks of the laid-out view to `layarSink` on every layout,
    /// so the last one before the photo is what the dump reads.
    func layarRecorded() -> some View {
        overlayPreferenceValue(LayarMarks.self) { marks in
            GeometryReader { proxy in
                let _ = layarSink.take(marks, proxy)
                Color.clear
            }
        }
    }
}

/// Every `layarKey` element of the view just captured in `rep`, as
/// scripts/screen_compare.py reads it. The marks carry no font, so `font` is
/// null, as on the desktop. `top` is how far below the top of `view` the
/// marked content starts.
func layarElements(in view: NSView, rep: NSBitmapImageRep, top: CGFloat, behind: [Int]) -> [[String: Any]] {
    let page = CGRect(origin: .zero, size: view.bounds.size)
    return layarSink.marks.compactMap { mark in
        let rect = mark.frame.offsetBy(dx: 0, dy: top)
        guard rect.width > 0, rect.height > 0, rect.intersects(page) else { return nil }
        let (fg, bg) = sampledColours(rep, rect, behind: behind)
        return ["key": mark.key,
                "frame": [rect.minX, rect.minY, rect.width, rect.height].map { Double($0) },
                "font": NSNull(), "fg": (fg as Any?) ?? NSNull(), "bg": (bg as Any?) ?? NSNull()]
    }
}

/// The main window's content as MainWindowController sets it up
/// (MainWindow.swift:22-38), at its default 696 x 600 points or at `size`.
/// A `sheet` is laid over the content's top centre, where macOS attaches it:
/// a real sheet opens a window of its own, which cacheDisplay leaves out.
func layarMain(_ model: AppModel, look: Look, size: NSSize = NSSize(width: 696, height: 600),
               sheet: AnyView? = nil) -> (NSWindow, NSView) {
    let host = NSHostingController(rootView: MainView(model: model)
        .overlay(alignment: .top) {
            if let sheet {
                // The radius and shadow are this file's window frame's (`framed`).
                sheet
                    .background(Color(nsColor: .windowBackgroundColor))
                    .clipShape(RoundedRectangle(cornerRadius: windowCornerRadius))
                    .shadow(color: Color(nsColor: look.shadow), radius: 18, y: 6)
            }
        }
        .layarRecorded())
    host.sizingOptions = [.minSize]
    let window = ShotWindow(contentRect: NSRect(x: 0, y: 0, width: 696, height: 600),
                            styleMask: [.titled, .resizable, .closable, .miniaturizable],
                            backing: .buffered, defer: false)
    window.appearance = look.appearance
    window.backgroundColor = look == .dark ? .windowBackgroundColor : .white
    window.contentViewController = host
    window.titleVisibility = .hidden
    window.titlebarAppearsTransparent = true
    window.setContentSize(size)
    return (window, host.view)
}

func layarPanel(_ model: AppModel, look: Look) -> (NSWindow, NSView) {
    let host = NSHostingView(rootView: MenuBarView(model: model).layarRecorded())
    host.frame = NSRect(origin: .zero, size: host.fittingSize)
    let window = ShotWindow(contentRect: host.frame, styleMask: [.borderless], backing: .buffered, defer: false)
    window.appearance = look.appearance
    window.backgroundColor = look.panelFill
    window.contentView = host
    return (window, host)
}

func layarEqualizer(look: Look) -> (NSWindow, NSView) {
    let window = ShotWindow(contentViewController: NSHostingController(rootView: EqualizerView(
        model: Mock.equalizer(), curveName: CurveNameDraft(), curvesFull: {}, askRename: { _ in }).layarRecorded()))
    window.appearance = look.appearance
    window.styleMask = [.titled, .closable]
    guard let content = window.contentView else { fatalError("The equalizer window has no content view") }
    return (window, content)
}

/// The track the desktop's layar_shots plays (`track`, `long_track`), so the
/// card's texts match by key.
func layarTrack(playing: Bool = true, art: Bool = true, long: Bool = false, shuffle: Bool = false,
                repeatMode: NowPlayingMonitor.RepeatMode = .off) -> NowPlayingMonitor.Track {
    NowPlayingMonitor.Track(
        bundleID: "com.example.player", appName: "Player",
        title: long ? "A track title long enough to need more than the two lines the card shows" : "A track title",
        artist: long ? "An artist name long enough to run past the end of its line" : "Artist",
        album: "Example Album", isPlaying: playing, artwork: art ? Mock.artwork() : nil,
        shuffle: shuffle, repeatMode: repeatMode)
}

/// The desktop's long speaker name (layar_shots.rs `LONG`).
let layarLong = "Living Room Speaker Next to the Big Window by the Bookshelves"

/// The desktop's MockBackend example (desktop/src/ui_model.rs) as the Mac
/// shows it, with streaming on and no track, as layar_shots' Scene starts:
/// the same speaker names, kinds, delays and switches, so both sides key the
/// same rows. Volumes are the core's defaults (core/src/volume.rs).
func layarModel(_ configure: (AppModel) -> Void = { _ in }) -> AppModel {
    let model = AppModel()
    model.running = true
    model.sources = [
        AppModel.Source(id: "", name: model.text.sourceSystem, icon: nil, isInput: false),
        AppModel.Source(id: "com.example.player", name: "Music Player", icon: nil, isInput: false),
        AppModel.Source(id: "com.example.browser", name: "Web Browser", icon: nil, isInput: false),
        AppModel.Source(id: "line-in", name: "Line In", icon: nil, isInput: true),
    ]
    model.local = [
        AppModel.Speaker(id: "speakers", name: "Speakers", tag: .builtIn, enabled: true, volume: 60, offsetMs: nil,
                         isReference: true),
        AppModel.Speaker(id: "headphones", name: "Headphones", tag: .builtIn, enabled: false, volume: 60, offsetMs: 0,
                         calibrationLeftOut: .headphones),
        AppModel.Speaker(id: "desk", name: "Desk Speaker", tag: .usb, enabled: false, volume: 60, offsetMs: 120),
        AppModel.Speaker(id: "headset", name: "Headset", tag: .bluetooth, enabled: false, volume: 60, offsetMs: -250),
    ]
    model.airplay = [
        AppModel.Speaker(id: "kitchen", name: "Kitchen", tag: .airplay, enabled: true, volume: 60, offsetMs: nil,
                         autoConnect: true),
        AppModel.Speaker(id: "living", name: "Living Room", tag: .airplay, enabled: false, volume: 60, offsetMs: nil,
                         availability: .needsCode),
        AppModel.Speaker(id: "bedroom", name: "Bedroom", tag: .airplay, enabled: false, volume: 60, offsetMs: nil),
    ]
    // The desktop example's speakers sit at 13 % in the system.
    model.deviceVolumes = ["speakers": .low(percent: 13)]
    model.builtInVolume = 0.13
    model.airPlayDelayMs = 1750
    configure(model)
    return model
}

/// Changes the example speaker `id`, local or AirPlay.
func layarEdit(_ model: AppModel, _ id: String, _ change: (inout AppModel.Speaker) -> Void) {
    if let at = model.local.firstIndex(where: { $0.id == id }) { change(&model.local[at]) }
    if let at = model.airplay.firstIndex(where: { $0.id == id }) { change(&model.airplay[at]) }
}

/// Gives the example speaker `id` the desktop's long name (layar_shots.rs `rename`).
func layarRename(_ model: AppModel, _ id: String) {
    func renamed(_ speaker: AppModel.Speaker) -> AppModel.Speaker {
        var copy = AppModel.Speaker(id: speaker.id, name: layarLong, tag: speaker.tag, enabled: speaker.enabled,
                                    volume: speaker.volume, offsetMs: speaker.offsetMs, autoConnect: speaker.autoConnect,
                                    availability: speaker.availability, systemName: layarLong)
        copy.isReference = speaker.isReference
        copy.calibrationLeftOut = speaker.calibrationLeftOut
        return copy
    }
    model.local = model.local.map { $0.id == id ? renamed($0) : $0 }
    model.airplay = model.airplay.map { $0.id == id ? renamed($0) : $0 }
}

let layarSmallest = NSSize(width: 696, height: 460)   // MainWindow.swift:320

/// The Calibrate All sheet over the main window, as MainView presents it
/// (MainWindow.swift:352-355, its tint `accent` at :136).
func layarCalibrateSheet(_ model: AppModel, look: Look, size: NSSize = NSSize(width: 696, height: 600)) -> (NSWindow, NSView) {
    layarMain(model, look: look, size: size,
              sheet: AnyView(CalibrateAllSheetView(model: model, room: size.height).tint(Brand.teal)))
}

/// The desktop's Calibrate All example (layar_shots.rs `calibrate_all`):
/// Desk Speaker and Headset on, and the reference's 13 % under "Before you start".
func layarCalibrateAll(_ change: (AppModel) -> Void = { _ in }) -> AppModel {
    layarModel { model in
        layarEdit(model, "desk") { $0.enabled = true }
        layarEdit(model, "headset") { $0.enabled = true }
        model.calibrateAll.volume = CalibrationVolumeCheck(DeviceVolumeReading(volume: 0.13, muted: false))
        change(model)
    }
}

/// The desktop's four result rows (layar_shots.rs `results`): the reference,
/// two clear measurements and an unclear one, AirPlay spread by 23 ms.
func layarResults(_ names: [String]) -> CalibrateAllSheet {
    func row(_ index: Int, _ kind: CalibrateAllKind, _ result: CalibrateAllSheet.Result) -> CalibrateAllSheet.Row {
        CalibrateAllSheet.Row(index: index, speaker: CalibrateAllSpeaker(id: "row\(index)", name: names[index], kind: kind,
                                                                         offsetMs: 2000, latencyMs: 0),
                              oldMs: 2000, result: result)
    }
    return CalibrateAllSheet(rows: [
        row(0, .local, .reference),
        row(1, .local, .certain(newMs: 132, uncertaintyMs: 1.2, clamped: false)),
        row(2, .airPlay, .certain(newMs: 2040, uncertaintyMs: 1.2, clamped: false)),
        row(3, .airPlay, .uncertain(newMs: 2040, uncertaintyMs: 14)),
    ], airPlay: CalibrateAllSheet.AirPlay(state: .spread, oldMs: 2000, newMs: 2040, spreadMs: 23, certainRows: 1))
}

let layarSheetNames = ["Speakers", "Desk Speaker", "Kitchen", "Bedroom"]

/// The headphones on and left out of Calibrate All (layar_shots.rs
/// `left_out`); `only`: they and the reference are the only speakers on.
func layarLeftOut(included: Bool, only: Bool = false, long: Bool = false) -> AppModel {
    layarModel { model in
        if long { layarRename(model, "headphones") }
        layarEdit(model, "headphones") { $0.enabled = true }
        if only {
            for at in model.local.indices { model.local[at].enabled = ["speakers", "headphones"].contains(model.local[at].id) }
            for at in model.airplay.indices { model.airplay[at].enabled = false }
            model.calibrateAll.blocker = .onlyLeftOut
        } else {
            layarEdit(model, "desk") { $0.enabled = true }
        }
        model.calibrateAll.volume = CalibrationVolumeCheck(DeviceVolumeReading(volume: 0.13, muted: false))
        model.calibrateAll.leftOut = [CalibrateAllLeftOut(id: "headphones", name: long ? layarLong : "Headphones",
                                                          reason: .headphones, included: included)]
    }
}

/// Desk Speaker on with its delay controls open, calibrating in `phase`
/// (layar_shots.rs `calibrating`).
func layarRowCalibration(_ phase: CalibrationPhase) -> AppModel {
    layarModel { model in
        layarEdit(model, "desk") { $0.enabled = true }
        model.calibration = CalibrationStatus(uid: "desk", phase: phase, holdsOutputs: phase.isRunning)
        model.toggleDelayPanel("desk")
    }
}

/// The Settings window as SettingsWindowController.makeWindow sets it up
/// (SettingsWindow.swift:33-46), as tall as its content.
func layarSettings(_ model: AppModel, look: Look) -> (NSWindow, NSView) {
    // The renderer runs on the main thread; UpdateChecks is @MainActor (SettingsWindow.swift:145).
    let updates = MainActor.assumeIsolated { UpdateChecks() }
    let host = NSHostingController(rootView: SettingsView(model: model, updates: updates, setUpPermissions: {},
                                                          onHeight: { _ in }).layarRecorded())
    host.sizingOptions = []
    let window = ShotWindow(contentViewController: host)
    window.appearance = look.appearance
    window.title = model.text.settingsTitle
    window.styleMask = [.titled, .closable]
    window.setContentSize(NSSize(width: SettingsView.width, height: host.sizeThatFits(in: PanelTopLayout.measuring).height))
    guard let content = window.contentView else { fatalError("The Settings window has no content view") }
    return (window, content)
}

/// The Web Remote window as WebRemoteWindowController.show sets it up
/// (WebRemoteWindow.swift:21-28), on the desktop's port 8770.
func layarRemote(look: Look, _ configure: (WebRemoteSettings) -> Void = { _ in }) -> (NSWindow, NSView) {
    let settings = WebRemoteSettings(enabled: false, port: 8770)
    configure(settings)
    let text = UIStrings()
    let window = ShotWindow(contentViewController: NSHostingController(rootView: WebRemoteView(
        settings: settings, text: text, announcer: WebRemoteAnnouncer()).layarRecorded()))
    window.appearance = look.appearance
    window.title = text.webRemoteTitle
    window.styleMask = [.titled, .closable]
    guard let content = window.contentView else { fatalError("The Web Remote window has no content view") }
    return (window, content)
}

/// The desktop's three presets (ui_model.rs `presets_preview`), Evening in use.
func layarPresets(_ model: AppModel) {
    model.presets = [
        PresetRow(id: "preset:1", name: "Evening", current: true, picked: true, speakers: 2, master: model.master),
        PresetRow(id: "preset:2", name: "Movie Night", speakers: 2, master: model.master),
        PresetRow(id: "preset:3", name: "Whole House", speakers: 5, master: model.master),
    ]
}

/// The LAYAR rows (notes IDs, shared with the desktop's layar_shots), each set
/// up as the desktop's row of the same ID sets up its example.
let layarRows: [(id: String, draw: (Look) -> (NSWindow, NSView))] = [
    ("MAIN-IDLE", { layarMain(layarModel { $0.running = false }, look: $0) }),
    ("MAIN-PLAYING", { layarMain(layarModel { $0.nowPlaying = layarTrack() }, look: $0) }),
    ("MAIN-HEADER", { layarMain(layarModel { $0.nowPlaying = layarTrack() }, look: $0) }),
    ("MAIN-HEADER-OFF", { layarMain(layarModel { $0.running = false }, look: $0) }),
    ("MAIN-VOLUME-ROWS", { layarMain(layarModel {
        layarEdit($0, "desk") { $0.enabled = true }
        $0.nowPlaying = layarTrack()
    }, look: $0) }),
    ("MAIN-AMBIENT-REST", { layarMain(layarModel { $0.nowPlaying = layarTrack(playing: false) }, look: $0) }),
    ("MAIN-NOWPLAYING-NOART", { layarMain(layarModel { $0.nowPlaying = layarTrack(art: false) }, look: $0) }),
    ("MAIN-NOWPLAYING-PAUSED", { layarMain(layarModel { $0.nowPlaying = layarTrack(playing: false, art: false) },
                                           look: $0) }),
    ("MAIN-SHUFFLE-OFF", { layarMain(layarModel { $0.nowPlaying = layarTrack() }, look: $0) }),
    ("MAIN-SHUFFLE-ON", { layarMain(layarModel { $0.nowPlaying = layarTrack(shuffle: true) }, look: $0) }),
    ("MAIN-REPEAT-OFF", { layarMain(layarModel { $0.nowPlaying = layarTrack() }, look: $0) }),
    ("MAIN-REPEAT-ALL", { layarMain(layarModel { $0.nowPlaying = layarTrack(repeatMode: .all) }, look: $0) }),
    ("MAIN-REPEAT-ONE", { layarMain(layarModel { $0.nowPlaying = layarTrack(repeatMode: .one) }, look: $0) }),
    ("MAIN-INPUT-LEVEL", { layarMain(layarModel { $0.sourceID = "line-in" }, look: $0) }),
    ("MAIN-LIVEINPUT-NOTE", { layarMain(layarModel { $0.sourceID = "line-in" }, look: $0) }),
    ("MAIN-MASTER-MUTED", { layarMain(layarModel { $0.toggleMasterMute() }, look: $0) }),
    ("MAIN-NETWORK-DIVIDERS", { layarMain(layarModel { $0.nowPlaying = layarTrack() }, look: $0) }),
    ("MAIN-LOCALNET-HINT", { layarMain(layarModel {
        $0.airplay = []
        $0.airplayFoundNone = true
        $0.localNetworkHint = .possible
    }, look: $0) }),
    ("MAIN-LONGNAME", { layarMain(layarModel {
        layarRename($0, "kitchen")
        $0.nowPlaying = layarTrack(long: true)
    }, look: $0) }),
    ("MAIN-EMPTY", { layarMain(layarModel {
        $0.local = []
        $0.airplay = []
    }, look: $0) }),
    ("MAIN-SMALLEST", { layarMain(layarModel(), look: $0, size: layarSmallest) }),
    ("MAIN-SMALLEST-PLAYING", { layarMain(layarModel { $0.nowPlaying = layarTrack() }, look: $0, size: layarSmallest) }),
    ("MAIN-SYSVOL-SMALLEST", { layarMain(layarModel { $0.toggleDelayPanel("speakers") }, look: $0, size: layarSmallest) }),
    ("MAIN-THISPC-NONE", { layarMain(layarModel { $0.local = [] }, look: $0) }),
    ("MAIN-ROW-OFF", { layarMain(layarModel { layarEdit($0, "kitchen") { $0.enabled = false } }, look: $0) }),
    ("MAIN-ROW-WILLPLAY", { layarMain(layarModel { $0.running = false }, look: $0) }),
    ("MAIN-ROW-LOWVOL", { layarMain(layarModel {
        layarEdit($0, "desk") { $0.enabled = true }
        $0.deviceVolumes["desk"] = .low(percent: 18)
    }, look: $0) }),
    ("MAIN-ROW-LOWVOL-HEADPHONES", { layarMain(layarModel {
        layarEdit($0, "headphones") { $0.enabled = true }
        $0.deviceVolumes["headphones"] = .low(percent: 10)
    }, look: $0) }),
    ("MAIN-ROW-MUTED-DEVICE", { layarMain(layarModel {
        layarEdit($0, "desk") { $0.enabled = true }
        $0.deviceVolumes["desk"] = .muted
    }, look: $0) }),
    ("MAIN-ROW-THISPC", { layarMain(layarModel { layarEdit($0, "speakers") { $0.enabled = true } }, look: $0) }),
    ("MAIN-ROW-THISPC-OFF", { layarMain(layarModel { layarEdit($0, "speakers") { $0.enabled = false } }, look: $0) }),
    // The desktop's "waiting for the device" and "not on the network" are the
    // Mac's reconnecting and unannounced AirPlay rows.
    ("MAIN-ROW-CONNECTING", { layarMain(layarModel { layarEdit($0, "kitchen") { $0.reconnecting = true } }, look: $0) }),
    ("MAIN-ROW-NEEDSCODE", { layarMain(layarModel(), look: $0) }),
    ("MAIN-ROW-CODE-EVERYTIME", { layarMain(layarModel {
        layarEdit($0, "bedroom") { $0.availability = .codeEveryTime }
    }, look: $0) }),
    ("MAIN-ROW-PROBLEM", { layarMain(layarModel { layarEdit($0, "kitchen") { $0.unannounced = true } }, look: $0) }),
    ("MAIN-ROW-DEVICE-TURNED-DOWN", { layarMain(layarModel {
        layarEdit($0, "desk") { $0.enabled = true }
        $0.deviceVolumes["desk"] = .mutedAndLow(percent: 5)
    }, look: $0) }),
    ("MAIN-DELAY-OPEN", { layarMain(layarModel {
        layarEdit($0, "desk") { $0.enabled = true }
        $0.toggleDelayPanel("desk")
    }, look: $0) }),
    ("MAIN-DELAY-ROWS", { layarMain(layarModel {
        layarEdit($0, "desk") { $0.enabled = true }
        layarEdit($0, "headset") { $0.enabled = true }
    }, look: $0) }),
    ("MAIN-AIRPLAY-DELAY-OPEN", { layarMain(layarModel(), look: $0) }),
    ("MAIN-SYSVOL-FOLDED", { layarMain(layarModel(), look: $0) }),
    ("MAIN-SYSVOL-OPEN", { layarMain(layarModel { $0.toggleDelayPanel("speakers") }, look: $0) }),
    ("MAIN-CAST-SEARCHING", { layarMain(layarModel { $0.castList = .searching }, look: $0) }),
    ("MAIN-CAST-NONEFOUND", { layarMain(layarModel { $0.castList = .noneFound }, look: $0) }),
    ("MAIN-CAST-FAILED", { layarMain(layarModel { $0.castList = .failed }, look: $0) }),
    ("NOTICE-SOURCE", { layarMain(layarModel {
        $0.notices.post($0.text.sourceTerminated(name: "Music Player"), subject: Notice.source)
    }, look: $0) }),
    ("TRAY-IDLE", { layarPanel(layarModel { $0.running = false }, look: $0) }),
    ("TRAY-PLAYING", { layarPanel(layarModel { $0.nowPlaying = layarTrack() }, look: $0) }),
    ("TRAY-EMPTY", { layarPanel(layarModel {
        $0.local = []
        $0.airplay = []
    }, look: $0) }),
    ("TRAY-LONGNAME", { layarPanel(layarModel {
        layarRename($0, "kitchen")
        $0.nowPlaying = layarTrack(long: true)
    }, look: $0) }),
    ("TRAY-ROW-OFF", { layarPanel(layarModel { layarEdit($0, "kitchen") { $0.enabled = false } }, look: $0) }),
    ("TRAY-ROW-LOWVOL", { layarPanel(layarModel {
        layarEdit($0, "desk") { $0.enabled = true }
        $0.deviceVolumes["desk"] = .low(percent: 18)
    }, look: $0) }),
    ("TRAY-ROW-THISPC", { layarPanel(layarModel(), look: $0) }),
    ("TRAY-ROW-HINT-TALL", { layarPanel(layarModel {
        layarRename($0, "speakers")
        $0.deviceVolumes["speakers"] = .mutedAndLow(percent: 8)
    }, look: $0) }),
    ("TRAY-TRANSPORT", { layarPanel(layarModel { $0.nowPlaying = layarTrack(shuffle: true, repeatMode: .all) },
                                    look: $0) }),
    ("TRAY-AMBIENT-REST", { layarPanel(layarModel { $0.nowPlaying = layarTrack(playing: false) }, look: $0) }),
    ("TRAY-MASTER-MUTED", { layarPanel(layarModel { $0.toggleMasterMute() }, look: $0) }),
    ("TRAY-NOTICE", { layarPanel(layarModel {
        $0.notices.post($0.text.sourceTerminated(name: "Music Player"), subject: Notice.source)
    }, look: $0) }),
    ("EQ-WINDOW", { layarEqualizer(look: $0) }),
    ("CAL-SHEET", { layarCalibrateSheet(layarCalibrateAll(), look: $0) }),
    ("CAL-SHEET-RUNNING", { layarCalibrateSheet(layarCalibrateAll {
        $0.calibrateAll.progress = .listening(name: "Desk Speaker", sweep: 1)
        $0.calibrateAll.holdsOutputs = true
    }, look: $0) }),
    ("CAL-SHEET-RESULT", { layarCalibrateSheet(layarCalibrateAll { $0.calibrateAll.sheet = layarResults(layarSheetNames) },
                                               look: $0) }),
    ("CAL-SHEET-FAILED", { layarCalibrateSheet(layarCalibrateAll {
        var sheet = layarResults(layarSheetNames)
        for at in 1..<sheet.rows.count { sheet.rows[at].result = .notMeasured }
        sheet.airPlay = nil
        sheet.ended = .failed(.sourceSilent)
        $0.calibrateAll.sheet = sheet
        $0.calibrateAll.line = .ended(.failed(.sourceSilent))
    }, look: $0) }),
    ("CAL-SHEET-LEFTOUT", { layarCalibrateSheet(layarLeftOut(included: false), look: $0) }),
    ("CAL-SHEET-INCLUDED", { layarCalibrateSheet(layarLeftOut(included: true), look: $0) }),
    ("CAL-SHEET-ONLY-LEFTOUT", { layarCalibrateSheet(layarLeftOut(included: false, only: true), look: $0) }),
    ("CAL-SHEET-LEFTOUT-LONG", { layarCalibrateSheet(layarLeftOut(included: false, long: true), look: $0) }),
    ("CAL-SHEET-SMALLEST", { layarCalibrateSheet(layarCalibrateAll { $0.calibrateAll.sheet = layarResults(layarSheetNames) },
                                                 look: $0, size: layarSmallest) }),
    ("MAIN-CALIBRATE-SHEET-NARROW", { layarCalibrateSheet(layarCalibrateAll {
        $0.calibrateAll.sheet = layarResults(layarSheetNames)
    }, look: $0, size: layarSmallest) }),
    ("CAL-SHEET-LONGNAME", { layarCalibrateSheet(layarCalibrateAll {
        $0.calibrateAll.sheet = layarResults(["Speakers", layarLong, "Kitchen", layarLong])
    }, look: $0) }),
    ("MAIN-ROW-CAL-RUNNING", { layarMain(layarRowCalibration(.running(.measuring(.chosen))), look: $0) }),
    ("MAIN-ROW-CAL-DONE", { layarMain(layarRowCalibration(.done(offsetMs: 132, clamped: false)), look: $0) }),
    ("MAIN-ROW-CAL-FAILED", { layarMain(layarRowCalibration(.failed(.sourceSilent)), look: $0) }),
    ("MAIN-ADD-ADDRESS", { look in
        let model = layarModel()
        return layarMain(model, look: look, sheet: AnyView(CastAddressSheet(model: model).tint(Brand.teal)))
    }),
    // Each notice as the Mac posts it for the desktop NoticeMessage of the
    // row (AppDelegate+Engine, +NativeAirPlay, +Cast, +Outputs, +Presets, +Zones).
    ("NOTICE-START", { layarMain(layarModel {
        $0.notices.post($0.text.chooseSpeakerBeforeStart, subject: Notice.start, shortName: $0.text.noSpeakerNoticeName)
    }, look: $0) }),
    ("NOTICE-CAPTURE", { layarMain(layarModel {
        $0.notices.post($0.text.captureNotWorking(nil), subject: Notice.capture)
    }, look: $0) }),
    ("NOTICE-LOGIN", { layarMain(layarModel {
        $0.notices.post(Notice.loginRefused(turningOn: true, $0.text), subject: Notice.login,
                        shortName: $0.text.launchAtLoginNoticeName(turningOn: true))
    }, look: $0) }),
    ("NOTICE-AIRPLAY-SENDER", { layarMain(layarModel {
        $0.notices.post($0.text.nativeAirPlayStopped, subject: Notice.airplaySender,
                        shortName: $0.text.airPlayStoppedNoticeName)
    }, look: $0) }),
    ("NOTICE-AIRPLAY-PTP", { layarMain(layarModel {
        $0.notices.post($0.text.nativeAirPlayPtpPortsTaken, subject: Notice.airplayPtp, shortName: $0.text.ptpPortsNoticeName)
    }, look: $0) }),
    ("NOTICE-PAIRINGS", { layarMain(layarModel {
        $0.notices.post($0.text.nativeAirPlayPairingsNotSaved, subject: Notice.pairings,
                        shortName: $0.text.pairingsNotSavedNoticeName)
    }, look: $0) }),
    ("NOTICE-PAIRINGS-IGNORED", { layarMain(layarModel {
        $0.notices.post($0.text.nativeAirPlayPairingsIgnored, subject: Notice.pairingsIgnored,
                        shortName: $0.text.pairingsIgnoredNoticeName)
    }, look: $0) }),
    ("NOTICE-ZONES", { layarMain(layarModel {
        $0.notices.post($0.text.groupsBecameZones, subject: Notice.zones, shortName: $0.text.groupsBecameZonesNoticeName,
                        info: true)
    }, look: $0) }),
    ("NOTICE-PRESETS", { layarMain(layarModel {
        let leftOff = [PresetNotice.LeftOff(name: "Patio", reason: .notFound)]
        $0.notices.post(PresetNotice.message(preset: "Whole House", leftOff: leftOff, nothingChanged: false, text: $0.text) ?? "",
                        subject: Notice.presets, title: PresetNotice.title(nothingChanged: false, text: $0.text),
                        shortName: $0.text.presetLeftOffNoticeName)
    }, look: $0) }),
    ("NOTICE-CAST-SENDER", { layarMain(layarModel {
        $0.notices.post($0.text.castProblem(.busy, name: "Porch"), subject: Notice.cast("porch"),
                        shortName: $0.text.cantPlayNoticeName("Porch"))
    }, look: $0) }),
    ("NOTICE-AIRPLAY-AHEAD", { layarMain(layarModel {
        $0.notices.post($0.text.airPlayAheadOfCast(tenths: 4, speaker: "Porch"), subject: Notice.airPlayAhead("porch"),
                        shortName: $0.text.airPlayAheadNoticeName("Porch"))
    }, look: $0) }),
    ("NOTICE-PERMISSION", { layarMain(layarModel {
        $0.notices.post($0.text.microphoneDenied, subject: Notice.start, shortName: $0.text.microphoneNoticeName,
                        settings: .microphone)
    }, look: $0) }),
    ("NOTICE-STACK", { layarMain(layarModel {
        $0.notices.post($0.text.sourceTerminated(name: "Music Player"), subject: Notice.source)
        $0.notices.post($0.text.captureNotWorking(nil), subject: Notice.capture)
        $0.notices.post($0.text.nativeAirPlayPtpPortsTaken, subject: Notice.airplayPtp, shortName: $0.text.ptpPortsNoticeName)
        $0.notices.post($0.text.groupsBecameZones, subject: Notice.zones, shortName: $0.text.groupsBecameZonesNoticeName,
                        info: true)
    }, look: $0) }),
    ("NOTICE-LONG", { layarMain(layarModel {
        $0.notices.post($0.text.nativeAirPlayTooManyAirPlay2(name: layarLong),
                        subject: SpeakerKey.make(id: "kitchen", airplay: true),
                        title: $0.text.nativeAirPlayWaitingTitle(name: layarLong))
    }, look: $0) }),
    ("MAIN-SETTINGS", { layarSettings(layarModel(), look: $0) }),
    ("SETTINGS-SWITCH-OFF", { layarSettings(layarModel {
        $0.launchAtLogin = false
        $0.resumesAfterSleep = false
        $0.volumeKeys = false
    }, look: $0) }),
    ("WEBREMOTE-OFF", { layarRemote(look: $0) }),
    ("WEBREMOTE-RUNNING", { layarRemote(look: $0) {
        $0.enabled = true
        $0.status = .running(port: 8770)
        $0.addresses = RemoteAddresses(localHostName: "den", ipv4: "192.0.2.10")
        $0.code = "4821"
        $0.deviceCount = 1
    } }),
    ("WEBREMOTE-FAILED", { layarRemote(look: $0) {
        $0.enabled = true
        // ASC_REMOTE_ERROR_CONFIG, which the window words as the desktop's generic "couldn't start".
        $0.status = .failed(error: 1, port: 8770)
    } }),
    ("MAIN-PRESETS", { layarMain(layarModel { layarPresets($0) }, look: $0) }),
    ("MAIN-PRESET-LIST", { layarMain(layarModel {
        layarPresets($0)
        $0.presetsListOpen = true
    }, look: $0) }),
    ("MAIN-PRESETS-EMPTY", { layarMain(layarModel { $0.presetsListOpen = true }, look: $0) }),
]

/// The desktop's rows the Mac renderer does not set up, and why; written to
/// not-set-up.txt as layar_shots writes its own, for scripts/screen_compare.py.
let layarNotSetUp: [(ids: [String], reason: String)] = [
    (["MAIN-ROW-HOVER", "MAIN-ROW-OFF-HOVER", "MAIN-TRANSPORT-HOVER", "MAIN-EQ-BUTTON-HOVER", "TRAY-HOVER",
      "TRAY-ROW-OFF-HOVER", "MAIN-ROW-FOCUS", "MAIN-SCROLLED", "MAIN-ZONE-PLACEHOLDER", "EQ-SLIDER-DRAG"],
     "needs a pointer, keyboard focus or scroll the renderer does not drive"),
    (["MAIN-ROW-MENU", "MAIN-ROW-MOVE-ZONE-MENU", "MAIN-SOURCE-MENU", "MAIN-SOURCE-MENU-LONG", "MAIN-GEAR-MENU",
      "MAIN-ZONE-MENU", "MAIN-PRESET-ROW-MENU", "TRAY-SOURCE-MENU", "EQ-CURVE-MENU"],
     "a native menu, which draws outside the window the renderer photographs"),
    (["MAIN-SOURCE-APPS", "MAIN-SOURCE-APPS-LONG", "MAIN-SOURCE-APPS-EMPTY", "MAIN-SOURCE-APPS-SMALLEST",
      "TRAY-SOURCE-APPS", "TRAY-SOURCE-APPS-LONG", "LEVEL-POPUP", "TRAY-PRESET-PICKER"],
     "a popover, which opens in its own window on a click the renderer does not make"),
    (["MAIN-ROW-RENAME", "MAIN-ZONE-RENAME", "MAIN-PRESET-RENAME", "MAIN-DELAY-EDIT"],
     "needs typing in a field the renderer does not drive"),
    (["MAIN-ADD-ADDRESS-ERROR"],
     "the reason shows only after Add looked the address up; the sheet keeps it in its own CastAddressDraft"),
    (["MAIN-ZONE-CARD", "MAIN-ZONE-COLLAPSED", "TRAY-ZONE-HEADER"],
     "the desktop's zones_preview example (a stereo pair, a Cast speaker, three zones) is not mirrored here yet"),
    (["MAIN-DELAY-LONGNAME", "MAIN-ROW-LOWVOL-LONGNAME", "MAIN-ROW-THISPC-LONG", "MAIN-ROW-HINT-LONG",
      "MAIN-ROW-HINT-TALL", "MAIN-ROW-CANTPLAY"],
     "not mirrored here yet"),
    (["SETTINGS-UPDATES"], "the Updates box shows only while the updater runs (AppUpdater.isRunning), which the renderer does not start"),
    (["GUIDE-WINDOW", "GUIDE-SCROLLED"],
     "the Guide is an AppKit text view that AppDelegate's private makeGuideWindow builds, with no layarKey marks"),
    (["WEBREMOTE-ANDROID"], "the phone choice is the view's own PhoneChoice, which only a click sets"),
    (["MAIN-AIRPLAY-DELAY-FOLDED"], "the desktop folds the row when it has no AirPlay delay; the Mac model always holds one (airPlayDelayMs)"),
    (["EQ-SMALLEST"], "the Mac equalizer window has one size (EqualizerWindow.swift:25, not resizable)"),
]

/// The fewest keyed elements MAIN-PLAYING may list: its header, source,
/// Now Playing card, sidebar buttons and speaker rows come to well over this.
let layarPlayingFloor = 20

/// Writes `<ID>-terang|gelap.png` and `.json` per row, unframed, at `scale`.
func writeLayar() {
    var playingCount = 0
    for (id, draw) in layarRows {
        for (look, theme) in [(Look.light, "terang"), (Look.dark, "gelap")] {
            NSApp.appearance = look.appearance
            layarSink.marks = []
            let (window, content) = draw(look)
            // The main window's transparent title bar is part of what the user
            // sees, as the desktop's own top row is: photograph from the window's top.
            let view = window.titlebarAppearsTransparent ? content.superview ?? content : content
            let placed = content.convert(content.bounds, to: view)
            let top = view.isFlipped ? placed.minY : view.bounds.height - placed.maxY
            var behind = [255, 255, 255]
            look.appearance.performAsCurrentDrawingAppearance {
                if let fill = window.backgroundColor.usingColorSpace(.sRGB) {
                    behind = [fill.redComponent, fill.greenComponent, fill.blueComponent].map { Int(($0 * 255).rounded()) }
                }
            }
            let rep = capture(view)
            write(rep, "\(id)-\(theme).png")
            let elements = layarElements(in: view, rep: rep, top: top, behind: behind)
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
    let notSetUp = layarNotSetUp.flatMap { row in row.ids.map { "\($0): \(row.reason)\n" } }.joined()
    let notSetUpURL = outputDirectory.appendingPathComponent("not-set-up.txt")
    do { try Data(notSetUp.utf8).write(to: notSetUpURL) } catch { fatalError("Could not write \(notSetUpURL.path): \(error)") }
    guard playingCount >= layarPlayingFloor else {
        FileHandle.standardError.write(Data("MAIN-PLAYING lists \(playingCount) keyed elements, fewer than \(layarPlayingFloor)\n".utf8))
        exit(1)
    }
}

// MARK: - Output

try FileManager.default.createDirectory(at: outputDirectory, withIntermediateDirectories: true)
print("Rendering into \(outputDirectory.path)")
if arguments.dropFirst(3).contains("--layar") {
    setenv("AUDIOSYNC_LAYAR", "1", 1)
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
