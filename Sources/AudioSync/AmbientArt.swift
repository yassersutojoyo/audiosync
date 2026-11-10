import AppKit
import CAudioSyncCore
import Combine
import SwiftUI

/// What the Now Playing art follows: the bands (`AmbientBands`), read every
/// `AmbientBands.poll`, and the tint's `Tide`, read every `ASC_METER_POLL_MS`,
/// while the main window or the panel is on screen. Its own object, like
/// `InputLevel`, so the polls redraw the art alone.
final class SoundTide: ObservableObject {
    static let shared = SoundTide()

    /// `Tide.env`, published only when it changes.
    @Published private(set) var env = 0.0
    /// `AmbientMode.live`.
    @Published private(set) var live = 0.0
    /// Bass, mid and treble levels, 0 to 1.
    @Published private(set) var bands = [0.0, 0.0, 0.0]
    @Published private(set) var onScreen = false
    private var tide = Tide()
    private var mode = AmbientMode()
    private var timer: Timer?
    private var meter: Float = 0
    private var sincePeak: TimeInterval = 0
    private var sinceLog: TimeInterval = 0

    /// Polls `peak` (the largest sample magnitude since the last call, nil
    /// while nothing is captured) and `bands` (the bass, mid and treble peaks
    /// since the last call, nil while streaming is off); nil stops polling
    /// and stills the art.
    func follow(_ read: (peak: () -> Float?, bands: () -> [Float]?)?) {
        timer?.invalidate()
        timer = nil
        guard let read else {
            tide = Tide()
            mode = AmbientMode()
            meter = 0
            sincePeak = 0
            sinceLog = 0
            if env != 0 { env = 0 }
            if live != 0 { live = 0 }
            if bands != [0, 0, 0] { bands = [0, 0, 0] }
            if onScreen { onScreen = false }
            return
        }
        if !onScreen { onScreen = true }
        let dt = AmbientBands.poll
        let peakDt = Double(ASC_METER_POLL_MS) / 1000
        let poll = Timer(timeInterval: dt, repeats: true) { [weak self] _ in
            guard let self else { return }
            sincePeak += dt
            if sincePeak + 1e-9 >= peakDt {
                sincePeak = 0
                if let reading = read.peak() {
                    meter = asc_meter_level(meter, reading)
                    tide.step(level: Double(meter), dt: peakDt)
                } else {
                    meter = 0
                    tide.step(level: nil, dt: peakDt)
                }
                if tide.env != env { env = tide.env }
            }
            let taken = read.bands()
            let peaks = taken ?? []
            let current = bands
            let next = current.indices.map { index in
                AmbientBands.follow(current[index], to: AmbientBands.level(peak: index < peaks.count ? peaks[index] : 0), dt: dt)
            }
            mode.step(loudest: next.max() ?? 0, dt: dt)
            if taken != nil { sinceLog += dt }
            if DebugTrace.enabled, sinceLog + 1e-9 >= 5 {
                sinceLog = 0
                let levels: [String] = next.map { String(format: "%.2f", $0) }
                let dbfs: [String] = peaks.map { String(format: "%.1f", 20 * log10(Double($0))) }
                let loudest = String(format: "%.2f", next.max() ?? 0)
                DebugTrace.log("[AudioSync] art bands \(levels), peaks dBFS \(dbfs), loudest \(loudest), live \(mode.live)")
            }
            if next != bands { bands = next }
            if mode.live != live { live = mode.live }
        }
        RunLoop.main.add(poll, forMode: .common)
        timer = poll
    }
}

/// Three strands in the cover's colours that spread and ripple with the
/// sound's bass, mid and treble while streaming captures it, and otherwise
/// breathe slowly in one colour that shows on the card. One still line with
/// Reduce Motion.
/// `density` scales how far the strands spread: 1 for the main window's hero,
/// less for a smaller one.
struct AmbientArt: View {
    @ObservedObject var tide: SoundTide
    let colors: [Color]
    var height: CGFloat = 22
    var density: CGFloat = 1
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        let still = reduceMotion
        let interval = AmbientBreath.frameInterval(onScreen: tide.onScreen, reduceMotion: still, live: tide.live)
        let lineOpacity = still ? 1 : AmbientBreath.opacity
        let tones = colors.compactMap { NSColor($0).usingColorSpace(.sRGB) }
            .map { BrandRGB(red: Double($0.redComponent), green: Double($0.greenComponent), blue: Double($0.blueComponent)) }
        let pick = AmbientBreath.line(cover: tones, dark: colorScheme == .dark, opacity: lineOpacity)
        let lineColor = Color(.sRGB, red: pick.red, green: pick.green, blue: pick.blue)
        let liveColors = (0..<3).map { index -> Color in
            let cover = tones.isEmpty ? BrandRGB(red: 0.5, green: 0.5, blue: 0.5) : tones[index % tones.count]
            let tone = AmbientBreath.tone(cover, dark: colorScheme == .dark)
            return Color(.sRGB, red: tone.red, green: tone.green, blue: tone.blue)
        }
        TimelineView(.animation(minimumInterval: interval, paused: interval == nil)) { context in
            Canvas { canvas, size in
                let time = context.date.timeIntervalSinceReferenceDate
                let live = still ? 0 : tide.live
                let breath = still ? 0 : AmbientBreath.spread(at: time)
                let idle = still ? 0 : 1 - live
                let layers = still ? (breath: 1.0, sound: 0.0) : AmbientBreath.layers(live: live)
                let scale = size.height / 22
                let style = StrokeStyle(lineWidth: CGFloat(AmbientBreath.stroke(live: live)), lineCap: .round)
                let fade = CGFloat(AmbientBreath.edgeFade)
                func faded(_ color: Color, _ opacity: Double) -> GraphicsContext.Shading {
                    .linearGradient(Gradient(stops: [.init(color: color.opacity(0), location: 0),
                                                     .init(color: color.opacity(opacity), location: fade),
                                                     .init(color: color.opacity(opacity), location: 1 - fade),
                                                     .init(color: color.opacity(0), location: 1)]),
                                    startPoint: .zero, endPoint: CGPoint(x: size.width, y: 0))
                }
                for index in AmbientBreath.waves.indices where !still || index == 1 {
                    let band = still ? 0 : min(1, tide.bands[index])
                    let spread = AmbientBreath.motion(env: band, live: live, breath: breath)
                    var path = Path()
                    for step in 0...60 {
                        let x = size.width * CGFloat(step) / 60
                        let taper = CGFloat(AmbientBreath.taper(x: Double(step) / 60))
                        let lift = CGFloat(AmbientBreath.lift(strand: index, x: Double(step) / 60, time: time,
                                                              spread: spread * Double(density), idle: idle, slot: Double(size.height)))
                        let y = min(21, max(1, 11 + taper * lift)) * scale
                        if step == 0 { path.move(to: CGPoint(x: x, y: y)) } else { path.addLine(to: CGPoint(x: x, y: y)) }
                    }
                    if layers.breath > 0 {
                        canvas.stroke(path, with: faded(lineColor, layers.breath), style: style)
                    }
                    if layers.sound > 0 {
                        canvas.stroke(path, with: faded(liveColors[index], layers.sound), style: style)
                    }
                }
            }
        }
        .frame(height: height)
        .accessibilityHidden(true)
    }
}

/// The cover's colour over a hero's fill: up to 9% while the art moves,
/// none at rest.
struct AmbientTint: View {
    @ObservedObject var tide: SoundTide
    let color: Color
    var cornerRadius: CGFloat = 12

    var body: some View {
        RoundedRectangle(cornerRadius: cornerRadius)
            .fill(color.opacity(0.09 * min(1, tide.env)))
            .accessibilityHidden(true)
    }
}

/// Three colours of a cover (its top, middle and bottom bands), for the art
/// and the tint; grey without a cover.
enum CoverColors {
    private static var last: (image: NSImage, colors: [Color])?

    static func of(_ image: NSImage?) -> [Color] {
        let neutral = [Color.gray, .gray, .gray]
        guard let image else { return neutral }
        if let last, last.image === image { return last.colors }
        let colors = bands(image) ?? neutral
        last = (image, colors)
        return colors
    }

    /// The image drawn into a 1 x 3 bitmap, top band first.
    private static func bands(_ image: NSImage) -> [Color]? {
        guard let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: 1, pixelsHigh: 3, bitsPerSample: 8,
                                         samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                                         colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0),
              let context = NSGraphicsContext(bitmapImageRep: rep) else { return nil }
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = context
        context.imageInterpolation = .high
        image.draw(in: NSRect(x: 0, y: 0, width: 1, height: 3), from: .zero, operation: .copy, fraction: 1)
        NSGraphicsContext.restoreGraphicsState()
        let colors = (0..<3).compactMap { rep.colorAt(x: 0, y: $0)?.usingColorSpace(.sRGB).map { Color(nsColor: $0) } }
        return colors.count == 3 ? colors : nil
    }
}
