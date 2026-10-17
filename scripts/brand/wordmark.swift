// Outlines the "audiosync" wordmark from Poppins Medium (brand/fonts) into SVG, so the
// logo renders the same everywhere without the font installed.
//
// Writes brand/audiosync-wordmark.svg (for light backgrounds) and
// brand/audiosync-wordmark-dark.svg (for dark backgrounds).
// Usage: swift scripts/brand/wordmark.swift   (run from the repository root)

import CoreText
import Foundation

let fontURL = URL(fileURLWithPath: "brand/fonts/Poppins-Medium.ttf")
let size: CGFloat = 200
let tracking: CGFloat = -0.02   // em
let text = "audiosync"
let split = 5                   // "audio" | "sync"
let variants: [(file: String, audio: String, sync: String)] = [
    ("brand/audiosync-wordmark.svg", "#292929", "#1C9F91"),
    ("brand/audiosync-wordmark-dark.svg", "#F6F6F6", "#63DAC2"),
]

guard let descriptor = CTFontManagerCreateFontDescriptorFromData(try Data(contentsOf: fontURL) as CFData) else {
    fatalError("Could not load \(fontURL.path)")
}
let font = CTFontCreateWithFontDescriptor(descriptor, size, nil)

// Lay the word out with CoreText, so kerning and tracking match what the app renders.
let attributed = NSAttributedString(string: text, attributes: [
    NSAttributedString.Key(kCTFontAttributeName as String): font,
    NSAttributedString.Key(kCTKernAttributeName as String): tracking * size,
])
let line = CTLineCreateWithAttributedString(attributed)
var glyphPaths: [(index: Int, path: CGPath)] = []
for run in CTLineGetGlyphRuns(line) as! [CTRun] {
    let count = CTRunGetGlyphCount(run)
    var glyphs = [CGGlyph](repeating: 0, count: count)
    var positions = [CGPoint](repeating: .zero, count: count)
    var indices = [CFIndex](repeating: 0, count: count)
    CTRunGetGlyphs(run, CFRange(location: 0, length: count), &glyphs)
    CTRunGetPositions(run, CFRange(location: 0, length: count), &positions)
    CTRunGetStringIndices(run, CFRange(location: 0, length: count), &indices)
    for k in 0..<count {
        var transform = CGAffineTransform(translationX: positions[k].x, y: positions[k].y)
        if let path = CTFontCreatePathForGlyph(font, glyphs[k], &transform) {
            glyphPaths.append((indices[k], path))
        }
    }
}

// Tight bounds of the ink; SVG's y axis points down, so flip around the top edge.
let bounds = glyphPaths.map(\.path.boundingBoxOfPath).reduce(CGRect.null) { $0.union($1) }
func fmt(_ v: CGFloat) -> String {
    var s = String(format: "%.2f", Double(v))
    while s.hasSuffix("0") { s.removeLast() }
    if s.hasSuffix(".") { s.removeLast() }
    return s == "-0" ? "0" : s
}
func point(_ p: CGPoint) -> String { "\(fmt(p.x - bounds.minX)) \(fmt(bounds.maxY - p.y))" }
func svgPath(_ path: CGPath) -> String {
    var d: [String] = []
    path.applyWithBlock { element in
        let e = element.pointee
        switch e.type {
        case .moveToPoint: d.append("M\(point(e.points[0]))")
        case .addLineToPoint: d.append("L\(point(e.points[0]))")
        case .addQuadCurveToPoint: d.append("Q\(point(e.points[0])) \(point(e.points[1]))")
        case .addCurveToPoint: d.append("C\(point(e.points[0])) \(point(e.points[1])) \(point(e.points[2]))")
        case .closeSubpath: d.append("Z")
        @unknown default: break
        }
    }
    return d.joined(separator: " ")
}
let audio = glyphPaths.filter { $0.index < split }.map { svgPath($0.path) }.joined(separator: " ")
let sync = glyphPaths.filter { $0.index >= split }.map { svgPath($0.path) }.joined(separator: " ")

for v in variants {
    let svg = """
    <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 \(fmt(bounds.width)) \(fmt(bounds.height))">
      <title>AudioSync</title>
      <!-- Outlined from Poppins Medium, © The Poppins Project Authors, SIL Open Font License 1.1 -->
      <path fill="\(v.audio)" d="\(audio)"/>
      <path fill="\(v.sync)" d="\(sync)"/>
    </svg>

    """
    try svg.write(toFile: v.file, atomically: true, encoding: .utf8)
    print("wrote \(v.file) (\(fmt(bounds.width)) x \(fmt(bounds.height)))")
}
