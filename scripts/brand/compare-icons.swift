// Compares generated icon files with the committed ones, for scripts/brand/check-icons.sh.
//
// Usage: swift compare-icons.swift <expected-dir> <actual-dir> <file>...
//
// For every file: both copies must exist and decode with ImageIO; they must hold the
// same number of images, each with the same pixel size; for an .icns file the element
// types must match in order, and for an .ico file the directory entries (size and bit
// depth). Byte-identical files pass at once. Otherwise each image is drawn into 8-bit
// sRGB RGBA and compared channel by channel: the mean difference may be at most
// MEAN_LIMIT and the largest at most MAX_LIMIT (out of 255), which absorbs resampling
// and encoder changes between macOS releases but not a different picture.
// Exits 1 on any mismatch.

import CoreGraphics
import Foundation
import ImageIO

let meanLimit = 1.0
let maxLimit = 32

let args = CommandLine.arguments
guard args.count >= 4 else {
    FileHandle.standardError.write(Data("usage: compare-icons.swift <expected-dir> <actual-dir> <file>...\n".utf8))
    exit(2)
}
let expectedDir = URL(fileURLWithPath: args[1])
let actualDir = URL(fileURLWithPath: args[2])

func bigEndian32(_ data: Data, _ offset: Int) -> Int {
    data[offset..<offset + 4].reduce(0) { $0 << 8 | Int($1) }
}

func littleEndian16(_ data: Data, _ offset: Int) -> Int {
    Int(data[offset]) | Int(data[offset + 1]) << 8
}

/// The element types of an .icns file, in file order.
func icnsTypes(_ data: Data) -> [String]? {
    guard data.count >= 8, String(decoding: data[0..<4], as: UTF8.self) == "icns" else { return nil }
    var types: [String] = []
    var offset = 8
    while offset + 8 <= data.count {
        let length = bigEndian32(data, offset + 4)
        guard length >= 8 else { return nil }
        types.append(String(decoding: data[offset..<offset + 4], as: UTF8.self))
        offset += length
    }
    return offset == data.count ? types : nil
}

/// The directory entries of an .ico file as "WxH@bpp" (0 in the header means 256).
func icoEntries(_ data: Data) -> [String]? {
    guard data.count >= 6, littleEndian16(data, 0) == 0, littleEndian16(data, 2) == 1 else { return nil }
    let count = littleEndian16(data, 4)
    guard data.count >= 6 + 16 * count else { return nil }
    return (0..<count).map { index in
        let entry = 6 + 16 * index
        let width = data[entry] == 0 ? 256 : Int(data[entry])
        let height = data[entry + 1] == 0 ? 256 : Int(data[entry + 1])
        return "\(width)x\(height)@\(littleEndian16(data, entry + 6))"
    }
}

/// Draws an image into 8-bit premultiplied RGBA in sRGB.
func rgba(_ image: CGImage) -> [UInt8]? {
    let width = image.width, height = image.height
    var pixels = [UInt8](repeating: 0, count: width * height * 4)
    guard let space = CGColorSpace(name: CGColorSpace.sRGB) else { return nil }
    let drawn = pixels.withUnsafeMutableBytes { buffer -> Bool in
        guard let context = CGContext(data: buffer.baseAddress, width: width, height: height, bitsPerComponent: 8,
                                      bytesPerRow: width * 4, space: space,
                                      bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return false }
        context.draw(image, in: CGRect(x: 0, y: 0, width: width, height: height))
        return true
    }
    return drawn ? pixels : nil
}

func images(_ url: URL) -> [CGImage]? {
    guard let source = CGImageSourceCreateWithURL(url as CFURL, nil) else { return nil }
    let count = CGImageSourceGetCount(source)
    guard count > 0 else { return nil }
    var result: [CGImage] = []
    for index in 0..<count {
        guard let image = CGImageSourceCreateImageAtIndex(source, index, nil) else { return nil }
        result.append(image)
    }
    return result
}

/// Returns the problems found for one file; empty when it matches.
func compare(_ name: String) -> [String] {
    let expectedURL = expectedDir.appendingPathComponent(name)
    let actualURL = actualDir.appendingPathComponent(name)
    guard let expected = try? Data(contentsOf: expectedURL) else { return ["the committed file is missing"] }
    guard let actual = try? Data(contentsOf: actualURL) else { return ["the script did not write it"] }
    if expected == actual {
        print("  \(name): byte-identical")
        return []
    }

    let ext = expectedURL.pathExtension.lowercased()
    if ext == "icns" {
        guard let want = icnsTypes(expected), let got = icnsTypes(actual) else { return ["not a readable icns file"] }
        if want != got { return ["icns element types \(got), expected \(want)"] }
    } else if ext == "ico" {
        guard let want = icoEntries(expected), let got = icoEntries(actual) else { return ["not a readable ico file"] }
        if want != got { return ["ico entries \(got), expected \(want)"] }
    }

    guard let wantImages = images(expectedURL) else { return ["the committed file does not decode"] }
    guard let gotImages = images(actualURL) else { return ["the generated file does not decode"] }
    guard wantImages.count == gotImages.count else {
        return ["\(gotImages.count) images, expected \(wantImages.count)"]
    }

    var problems: [String] = []
    var worstMean = 0.0, worstMax = 0
    for (index, (want, got)) in zip(wantImages, gotImages).enumerated() {
        guard want.width == got.width, want.height == got.height else {
            problems.append("image \(index) is \(got.width)x\(got.height), expected \(want.width)x\(want.height)")
            continue
        }
        guard let a = rgba(want), let b = rgba(got) else {
            problems.append("image \(index) could not be drawn")
            continue
        }
        var total = 0, largest = 0
        for i in 0..<a.count {
            let difference = abs(Int(a[i]) - Int(b[i]))
            total += difference
            largest = max(largest, difference)
        }
        let mean = Double(total) / Double(a.count)
        worstMean = max(worstMean, mean)
        worstMax = max(worstMax, largest)
        if mean > meanLimit || largest > maxLimit {
            problems.append(String(format: "image %d (%dx%d) differs: mean %.3f, max %d", index, want.width,
                                   want.height, mean, largest))
        }
    }
    if problems.isEmpty {
        print(String(format: "  %@: %d image(s) match, mean difference %.3f, max %d", name, wantImages.count,
                     worstMean, worstMax))
    }
    return problems
}

var failed = false
for name in args.dropFirst(3) {
    for problem in compare(name) {
        print("  \(name): \(problem)")
        failed = true
    }
}
exit(failed ? 1 : 0)
