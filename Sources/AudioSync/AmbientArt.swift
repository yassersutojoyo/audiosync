import AppKit
import CAudioSyncCore
import Combine
import SwiftUI

/// What the Now Playing art follows (`Tide`), polled every
/// `ASC_METER_POLL_MS` while the main window or the panel is on screen. Its own object,
/// like `InputLevel`, so the polls redraw the art alone.
final class SoundTide: ObservableObject {
    static let shared = SoundTide()

    /// `Tide.env`, published only when it changes.
    @Published private(set) var env = 0.0
    /// `AmbientMode.live`.
    @Published private(set) var live = 0.0
    @Published private(set) var onScreen = false
    private var tide = Tide()
    private var mode = AmbientMode()
    private var timer: Timer?
    private var meter: Float = 0

    /// Polls `peak` (the largest sample magnitude since the last call, nil
    /// while nothing is captured); nil stops polling and stills the art.
    func follow(_ peak: (() -> Float?)?) {
        timer?.invalidate()
        timer = nil
        guard let peak else {
            tide = Tide()
            mode = AmbientMode()
            meter = 0
            if env != 0 { env = 0 }
            if live != 0 { live = 0 }
            if onScreen { onScreen = false }
            return
        }
        if !onScreen { onScreen = true }
        let dt = Double(ASC_METER_POLL_MS) / 1000
        let poll = Timer(timeInterval: dt, repeats: true) { [weak self] _ in
            guard let self else { return }
            if let reading = peak() {
                meter = asc_meter_level(meter, reading)
                AmbientMode.poll(&tide, &mode, meter: Double(meter), dt: dt)
            } else {
                meter = 0
                AmbientMode.poll(&tide, &mode, meter: nil, dt: dt)
            }
            if tide.env != env { env = tide.env }
            if mode.live != live { live = mode.live }
        }
        RunLoop.main.add(poll, forMode: .common)
        timer = poll
    }
}

/// Three strands in the cover's colours that spread and ripple with the
/// sound while streaming captures it, and otherwise breathe slowly in one
/// colour that shows on the card. One still line with Reduce Motion.
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
        TimelineView(.animation(minimumInterval: interval, paused: interval == nil)) { context in
            Canvas { canvas, size in
                let time = context.date.timeIntervalSinceReferenceDate
                let live = still ? 0 : tide.live
                let env = still ? 0 : min(1, tide.env)
                let breath = still ? 0 : AmbientBreath.spread(at: time)
                let spread = AmbientBreath.motion(env: env, live: live, breath: breath)
                let idle = still ? 0 : 1 - live
                let layers = still ? (breath: 1.0, sound: 0.0) : AmbientBreath.layers(live: live, env: env)
                let scale = size.height / 22
                for index in AmbientBreath.waves.indices where !still || index == 1 {
                    var path = Path()
                    for step in 0...60 {
                        let x = size.width * CGFloat(step) / 60
                        // Pinned at both ends, widest in the middle.
                        let taper = sin(.pi * CGFloat(step) / 60)
                        let lift = CGFloat(AmbientBreath.lift(strand: index, x: Double(step) / 60, time: time,
                                                              spread: spread * Double(density), idle: idle, slot: Double(size.height)))
                        let y = min(21, max(1, 11 + taper * lift)) * scale
                        if step == 0 { path.move(to: CGPoint(x: x, y: y)) } else { path.addLine(to: CGPoint(x: x, y: y)) }
                    }
                    let style = StrokeStyle(lineWidth: 1.6, lineCap: .round)
                    if layers.breath > 0 {
                        canvas.stroke(path, with: .color(lineColor.opacity(layers.breath)), style: style)
                    }
                    if layers.sound > 0 {
                        let color = colors.isEmpty ? Color.gray : colors[index % colors.count]
                        canvas.stroke(path, with: .color(color.opacity(layers.sound)), style: style)
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
