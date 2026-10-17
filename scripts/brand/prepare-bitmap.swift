// Turns the raster reference of the logo mark into a clean bitmap for potrace.
//
// The reference is upsampled, converted to the mark's coverage over the tile
// background, smoothed slightly (removing compression noise along the edges, which
// would otherwise cost potrace hundreds of extra curve segments) and thresholded at 50%.
//
// Usage: swift prepare-bitmap.swift <reference.png> <out.pgm> [scale=4] [sigma=3]

import AppKit
import CoreImage

let args = CommandLine.arguments
guard args.count >= 3 else {
    FileHandle.standardError.write(Data("usage: prepare-bitmap.swift <reference.png> <out.pgm> [scale] [sigma]\n".utf8))
    exit(2)
}
let scale = args.count > 3 ? Int(args[3])! : 4
let sigma = args.count > 4 ? Double(args[4])! : 3

guard let source = NSBitmapImageRep(data: try Data(contentsOf: URL(fileURLWithPath: args[1])))?.cgImage else {
    fatalError("Could not read \(args[1])")
}
let width = source.width * scale, height = source.height * scale

// Upsample into an sRGB buffer (rows top-down).
var rgba = [UInt8](repeating: 0, count: width * height * 4)
let srgb = CGColorSpace(name: CGColorSpace.sRGB)!
let upsampled = CGContext(data: &rgba, width: width, height: height, bitsPerComponent: 8, bytesPerRow: width * 4,
                          space: srgb, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
upsampled.interpolationQuality = .high
upsampled.draw(source, in: CGRect(x: 0, y: 0, width: width, height: height))

// Coverage: project each pixel onto the line from the background color (the four
// corners) to the mark color. 0 = background, 1 = mark; stored inverted (mark = black).
func pixel(_ i: Int) -> SIMD3<Double> { SIMD3(Double(rgba[i * 4]), Double(rgba[i * 4 + 1]), Double(rgba[i * 4 + 2])) }
let corners = [0, width - 1, (height - 1) * width, height * width - 1].map(pixel)
let background = corners.reduce(SIMD3<Double>(repeating: 0), +) / 4
let ink = SIMD3<Double>(28, 159, 145)   // #1C9F91
let direction = ink - background
let lengthSquared = (direction * direction).sum()
var coverage = [UInt8](repeating: 255, count: width * height)
for i in 0..<(width * height) {
    let t = min(1, max(0, ((pixel(i) - background) * direction).sum() / lengthSquared))
    coverage[i] = UInt8(((1 - t) * 255).rounded())
}

// Gaussian blur, then threshold at 50%.
let gray = CGColorSpaceCreateDeviceGray()
let coverageImage = CGContext(data: &coverage, width: width, height: height, bitsPerComponent: 8, bytesPerRow: width,
                              space: gray, bitmapInfo: CGImageAlphaInfo.none.rawValue)!.makeImage()!
var image = CIImage(cgImage: coverageImage)
let extent = image.extent
if sigma > 0 { image = image.clampedToExtent().applyingGaussianBlur(sigma: sigma).cropped(to: extent) }
var smoothed = [UInt8](repeating: 0, count: width * height)
CIContext(options: [.workingColorSpace: NSNull(), .outputColorSpace: NSNull()])
    .render(image, toBitmap: &smoothed, rowBytes: width, bounds: extent, format: .L8, colorSpace: nil)
for i in 0..<smoothed.count { smoothed[i] = smoothed[i] < 128 ? 0 : 255 }

// Binary PGM (P5), rows top-down, as potrace expects.
var pgm = Data("P5\n\(width) \(height)\n255\n".utf8)
pgm.append(contentsOf: smoothed)
try pgm.write(to: URL(fileURLWithPath: args[2]))
print("prepared \(args[2]) (\(width)x\(height), background \(background), sigma \(sigma))")
