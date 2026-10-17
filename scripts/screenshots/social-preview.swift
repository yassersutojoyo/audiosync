// Renders docs/images/social-preview.png, the 1280 × 640 image that link previews
// (the GitHub social preview, the site's og:image) show for AudioSync.
//
// Own work: it is composed only from the repository's brand files (the mark and
// wordmark in brand/, the Poppins Medium font in brand/fonts, SIL OFL 1.1) and the
// app's own main-window screenshot rendered by render.swift. scripts/screenshots/
// render.sh runs it after the screenshots; to run it on its own, from the repository
// root (needs macOS 14 or later to read the SVGs):
//
//   swift scripts/screenshots/social-preview.swift [main-window-light.png] [output.png]
//
// Defaults: docs/images/main-window-light.png and docs/images/social-preview.png.

import AppKit

let arguments = CommandLine.arguments
let screenshotPath = arguments.count > 1 ? arguments[1] : "docs/images/main-window-light.png"
let outputPath = arguments.count > 2 ? arguments[2] : "docs/images/social-preview.png"

let width = 1280
let height = 640

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data("social-preview: \(message)\n".utf8))
    exit(1)
}

func color(_ hex: UInt32, _ alpha: CGFloat = 1) -> NSColor {
    NSColor(srgbRed: CGFloat((hex >> 16) & 0xFF) / 255,
            green: CGFloat((hex >> 8) & 0xFF) / 255,
            blue: CGFloat(hex & 0xFF) / 255,
            alpha: alpha)
}

func image(_ path: String) -> NSImage {
    guard let image = NSImage(contentsOfFile: path), image.size.width > 0 else { fail("could not read \(path)") }
    return image
}

var fontError: Unmanaged<CFError>?
guard CTFontManagerRegisterFontsForURL(URL(fileURLWithPath: "brand/fonts/Poppins-Medium.ttf") as CFURL, .process, &fontError),
      let poppins = NSFont(name: "Poppins-Medium", size: 40) else {
    fail("could not load brand/fonts/Poppins-Medium.ttf (run from the repository root)")
}

let mark = image("brand/audiosync-mark.svg")
let wordmark = image("brand/audiosync-wordmark.svg")
let screenshot = image(screenshotPath)

guard let bitmap = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: width, pixelsHigh: height,
                                    bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                                    colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0),
      let context = NSGraphicsContext(bitmapImageRep: bitmap) else { fail("could not create the bitmap") }

// Draw top-down: y grows downward, like the site's layout.
let cg = context.cgContext
cg.translateBy(x: 0, y: CGFloat(height))
cg.scaleBy(x: 1, y: -1)
NSGraphicsContext.current = NSGraphicsContext(cgContext: cg, flipped: true)
cg.interpolationQuality = .high

// Background: the site's base color with its soft brand-color glows (site/index.html, .mesh).
color(0xF1F7F6).setFill()
CGRect(x: 0, y: 0, width: width, height: height).fill()
let glows: [(x: CGFloat, y: CGFloat, radius: CGFloat, color: NSColor)] = [
    (130, 110, 520, color(0x1C9F91, 0.22)),
    (1150, 60, 560, color(0x8FD3E8, 0.40)),
    (1060, 600, 600, color(0x63DAC2, 0.32)),
    (220, 640, 520, color(0xB5ECE2, 0.55)),
]
for glow in glows {
    let gradient = NSGradient(colors: [glow.color, glow.color.withAlphaComponent(0)])!
    gradient.draw(fromCenter: NSPoint(x: glow.x, y: glow.y), radius: 0,
                  toCenter: NSPoint(x: glow.x, y: glow.y), radius: glow.radius, options: [])
}

func draw(_ image: NSImage, in rect: CGRect) {
    image.draw(in: rect, from: .zero, operation: .sourceOver, fraction: 1, respectFlipped: true, hints: nil)
}

// Right: the main window, with its own shadow from the screenshot.
let shotWidth: CGFloat = 690
let shotHeight = shotWidth * screenshot.size.height / screenshot.size.width
draw(screenshot, in: CGRect(x: CGFloat(width) - shotWidth - 22, y: (CGFloat(height) - shotHeight) / 2,
                            width: shotWidth, height: shotHeight))

// Left: mark and wordmark, the tagline, and what it plays to.
let left: CGFloat = 72
let column: CGFloat = 470
let markHeight: CGFloat = 60
let markWidth = markHeight * mark.size.width / mark.size.height
draw(mark, in: CGRect(x: left, y: 92, width: markWidth, height: markHeight))
let wordHeight: CGFloat = 46
let wordWidth = wordHeight * wordmark.size.width / wordmark.size.height
draw(wordmark, in: CGRect(x: left + markWidth + 16, y: 92 + markHeight - wordHeight * 0.80,
                          width: wordWidth, height: wordHeight))

func paragraph(_ lineHeight: CGFloat) -> NSParagraphStyle {
    let style = NSMutableParagraphStyle()
    style.minimumLineHeight = lineHeight
    style.maximumLineHeight = lineHeight
    style.lineBreakMode = .byWordWrapping
    return style
}

let tagline = NSMutableAttributedString(
    string: "Free, open-source\nmulti-room audio\nfor Mac,",
    attributes: [.font: poppins.withSize(42), .foregroundColor: color(0x292929),
                 .kern: -0.6, .paragraphStyle: paragraph(54)])
tagline.append(NSAttributedString(
    string: "\nan Airfoil alternative",
    attributes: [.font: poppins.withSize(42), .foregroundColor: color(0x147167),
                 .kern: -0.6, .paragraphStyle: paragraph(54)]))
tagline.draw(with: CGRect(x: left, y: 214, width: column, height: 240),
             options: [.usesLineFragmentOrigin])

let detail = NSAttributedString(
    string: "HomePod · AirPlay 2 · local speakers",
    attributes: [.font: poppins.withSize(22), .foregroundColor: color(0x525C5A),
                 .paragraphStyle: paragraph(30)])
detail.draw(with: CGRect(x: left, y: 498, width: column, height: 60), options: [.usesLineFragmentOrigin])

NSGraphicsContext.current = nil

// Opaque 8-bit RGB keeps the file small.
guard let rendered = bitmap.cgImage,
      let opaque = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0,
                             space: CGColorSpace(name: CGColorSpace.sRGB)!,
                             bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue) else { fail("could not flatten the image") }
opaque.draw(rendered, in: CGRect(x: 0, y: 0, width: width, height: height))
guard let flat = opaque.makeImage(),
      let png = NSBitmapImageRep(cgImage: flat).representation(using: .png, properties: [:]) else {
    fail("could not encode the PNG")
}
do {
    try png.write(to: URL(fileURLWithPath: outputPath))
} catch {
    fail("could not write \(outputPath): \(error.localizedDescription)")
}
print("social-preview: wrote \(outputPath) (\(width)×\(height), \(png.count / 1024) KB)")
