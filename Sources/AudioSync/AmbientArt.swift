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
    private var tide = Tide()
    private var timer: Timer?
    private var meter: Float = 0

    /// Polls `peak` (the largest sample magnitude since the last call, nil
    /// while nothing is captured); nil stops polling and stills the art.
    func follow(_ peak: (() -> Float?)?) {
        timer?.invalidate()
        timer = nil
        guard let peak else {
            tide = Tide()
            meter = 0
            if env != 0 { env = 0 }
            return
        }
        let dt = Double(ASC_METER_POLL_MS) / 1000
        let poll = Timer(timeInterval: dt, repeats: true) { [weak self] _ in
            guard let self else { return }
            if let reading = peak() {
                meter = asc_meter_level(meter, reading)
                tide.step(level: Double(meter), dt: dt)
            } else {
                meter = 0
                tide.step(level: nil, dt: dt)
            }
            if tide.env != env { env = tide.env }
        }
        RunLoop.main.add(poll, forMode: .common)
        timer = poll
    }
}

/// Three strands in the cover's colours that spread and ripple as the sound
/// changes and rest as one thin line when it doesn't. Still with Reduce
/// Motion. `density` scales how far the strands spread: 1 for the main
/// window's hero, less for a smaller one.
struct AmbientArt: View {
    @ObservedObject var tide: SoundTide
    let colors: [Color]
    var height: CGFloat = 22
    var density: CGFloat = 1
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private static let waves: [(amplitude: CGFloat, length: CGFloat, offset: Double)] =
        [(7, 92, 0), (5, 61, 1.7), (3.2, 137, 3.1)]

    var body: some View {
        let env = reduceMotion ? 0 : CGFloat(min(1, tide.env))
        TimelineView(.animation(paused: env == 0)) { context in
            Canvas { canvas, size in
                let phase = context.date.timeIntervalSinceReferenceDate * 2
                let scale = size.height / 22
                for (index, wave) in Self.waves.enumerated() {
                    let opacity = env > 0 ? min(1, 0.25 + env * 2.5) : (index == 1 ? 0.35 : 0)
                    guard opacity > 0 else { continue }
                    var path = Path()
                    for step in 0...60 {
                        let x = size.width * CGFloat(step) / 60
                        // Pinned at both ends, widest in the middle.
                        let taper = sin(.pi * CGFloat(step) / 60)
                        let angle = 2 * .pi * Double(300 * CGFloat(step) / 60 / wave.length)
                            + phase * (1 + Double(index) * 0.35) + wave.offset
                        let lift = wave.amplitude * env * CGFloat(sin(angle)) + CGFloat(index - 1) * 2.2 * env
                        let y = min(21, max(1, 11 + taper * lift * density)) * scale
                        if step == 0 { path.move(to: CGPoint(x: x, y: y)) } else { path.addLine(to: CGPoint(x: x, y: y)) }
                    }
                    let color = colors.isEmpty ? Color.gray : colors[index % colors.count]
                    canvas.stroke(path, with: .color(color.opacity(opacity)),
                                  style: StrokeStyle(lineWidth: 1.6, lineCap: .round))
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
