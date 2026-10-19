import AppKit
import ImageIO
import UniformTypeIdentifiers

/// What AirPlay receivers with a screen, such as a TV, show while AudioSync
/// streams to them (`ReceiverInfo` decides it): the track's title, artist and
/// album, and its artwork as a JPEG that AudioSync's own sender hands on
/// (`NativeAirPlay.setTrack`, `setArtwork`).
enum TrackMetadata {

    struct Fields: Equatable {
        var title: String
        var artist: String
        var album: String
    }

    /// At most this many bytes of JPEG per image: a cover that every receiver
    /// takes at once, well under the 512 KiB the sender accepts (P125).
    static let artworkLimit = 6060

    // MARK: - Artwork

    /// At most this many pixels a side, as large as a receiver shows a cover.
    static let artworkSide = 600
    private static let smallestSide = 64
    private static let qualities = [0.65, 0.5, 0.4]

    /// `image` as a JPEG of at most `maxBytes`, as large and sharp as fits: the
    /// largest side up to `maxSide`, scaled down until it fits. Nil if not even
    /// the smallest fits. Any thread.
    static func jpeg(_ image: NSImage, maxSide: Int = artworkSide, maxBytes: Int = artworkLimit) -> Data? {
        var rect = CGRect(x: 0, y: 0, width: maxSide, height: maxSide)
        guard let cgImage = image.cgImage(forProposedRect: &rect, context: nil, hints: nil) else { return nil }
        return jpeg(cgImage, maxSide: maxSide, maxBytes: maxBytes)
    }

    static func jpeg(_ image: CGImage, maxSide: Int = artworkSide, maxBytes: Int = artworkLimit) -> Data? {
        guard maxBytes > 0, maxSide > 0,
              let first = scale(image, longestSide: min(maxSide, max(image.width, image.height)))
        else { return nil }
        // Smaller tries scale this one down, not the original, which can be large.
        let longest = max(first.width, first.height)
        var side = longest
        while true {
            guard let scaled = side == longest ? first : scale(first, longestSide: side) else { return nil }
            var smallest = Int.max
            for quality in qualities {
                guard let data = encodeJPEG(scaled, quality: quality) else { return nil }
                if data.count <= maxBytes { return data }
                smallest = min(smallest, data.count)
            }
            if side <= smallestSide { return nil }
            // The size grows about with the pixel count: aim at a side that
            // should fit, but always step down.
            let aim = Int(Double(side) * (Double(maxBytes) / Double(smallest)).squareRoot())
            side = max(min(aim, side * 85 / 100), smallestSide)
        }
    }

    /// Drawn on black: JPEG has no transparency.
    private static func scale(_ image: CGImage, longestSide side: Int) -> CGImage? {
        let longest = max(image.width, image.height)
        guard longest > 0, side > 0 else { return nil }
        let width = max(1, image.width * side / longest)
        let height = max(1, image.height * side / longest)
        guard let space = CGColorSpace(name: CGColorSpace.sRGB),
              let context = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8,
                                      bytesPerRow: 0, space: space,
                                      bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue) else { return nil }
        let bounds = CGRect(x: 0, y: 0, width: width, height: height)
        context.setFillColor(CGColor(gray: 0, alpha: 1))
        context.fill(bounds)
        context.interpolationQuality = .high
        context.draw(image, in: bounds)
        return context.makeImage()
    }

    private static func encodeJPEG(_ image: CGImage, quality: Double) -> Data? {
        let data = NSMutableData()
        guard let destination = CGImageDestinationCreateWithData(data, UTType.jpeg.identifier as CFString, 1, nil)
        else { return nil }
        CGImageDestinationAddImage(destination, image,
                                   [kCGImageDestinationLossyCompressionQuality: quality] as CFDictionary)
        return CGImageDestinationFinalize(destination) ? data as Data : nil
    }
}
