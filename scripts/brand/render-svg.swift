// Renders an SVG to a square PNG with AppKit's SVG support.
//
// Usage: swift render-svg.swift <in.svg> <out.png> <size> [--icon-shadow]
//   --icon-shadow adds the soft drop shadow macOS app icons carry under the tile.

import AppKit

let args = CommandLine.arguments
guard args.count >= 4, let size = Int(args[3]) else {
    FileHandle.standardError.write(Data("usage: render-svg.swift <in.svg> <out.png> <size> [--icon-shadow]\n".utf8))
    exit(2)
}
guard let image = NSImage(contentsOf: URL(fileURLWithPath: args[1])) else { fatalError("Could not read \(args[1])") }
let shadow = args.dropFirst(4).contains("--icon-shadow")

guard let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size, bitsPerSample: 8,
                                 samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                                 bytesPerRow: 0, bitsPerPixel: 0) else { fatalError("Could not allocate the bitmap") }
rep.size = NSSize(width: size, height: size)
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
NSGraphicsContext.current?.imageInterpolation = .high
if shadow {
    let s = NSShadow()
    let unit = CGFloat(size) / 1024
    s.shadowColor = NSColor.black.withAlphaComponent(0.28)
    s.shadowOffset = NSSize(width: 0, height: -12 * unit)
    s.shadowBlurRadius = 24 * unit
    s.set()
}
image.draw(in: NSRect(x: 0, y: 0, width: size, height: size))
NSGraphicsContext.restoreGraphicsState()

guard let png = rep.representation(using: .png, properties: [:]) else { fatalError("Could not encode the PNG") }
try png.write(to: URL(fileURLWithPath: args[2]))
