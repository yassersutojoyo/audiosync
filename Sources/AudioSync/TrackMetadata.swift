import AppKit
import ImageIO
import UniformTypeIdentifiers

/// What AirPlay receivers with a screen, such as a TV, show while AudioSync
/// streams to them (`ReceiverInfo` decides it): the track's title, artist and
/// album, and its artwork as a large and a small JPEG that AudioSync's own
/// sender hands on (`NativeAirPlay.setTrackInfo`), picking one per receiver.
enum TrackMetadata {

    struct Fields: Equatable {
        var title: String
        var artist: String
        var album: String
    }

    // MARK: - Artwork

    struct Limits: Equatable {
        /// Pixels on the longest side.
        let side: Int
        let bytes: Int
    }

    /// For receivers on the AirPlay 2 path, where a TV shows the cover large
    /// (S272).
    static let largeArtwork = Limits(side: 1024, bytes: 128 << 10)
    /// For receivers on the AirPlay 1 path, and the second try when one
    /// refuses the large image (P185).
    static let smallArtwork = Limits(side: 600, bytes: 40 << 10)
    private static let smallestSide = 64
    private static let qualities = [0.65, 0.5, 0.4]

    /// A cover in both sizes; the same bytes twice when the large one is
    /// within the small limits.
    struct Artwork: Equatable {
        let large: Data
        let small: Data
    }

    /// `image` as a large and a small JPEG, each as large and sharp as its
    /// limits allow; the small one serves as both when the large one can't be
    /// made. Nil if the small one can't be made either. Any thread.
    static func artwork(_ image: NSImage) -> Artwork? {
        var rect = CGRect(x: 0, y: 0, width: largeArtwork.side, height: largeArtwork.side)
        guard let cgImage = image.cgImage(forProposedRect: &rect, context: nil, hints: nil) else { return nil }
        return artwork(cgImage)
    }

    static func artwork(_ image: CGImage) -> Artwork? {
        let large = fit(image, maxSide: largeArtwork.side, maxBytes: largeArtwork.bytes)
        if let large, large.side <= smallArtwork.side && large.data.count <= smallArtwork.bytes {
            return Artwork(large: large.data, small: large.data)
        }
        guard let small = fit(image, maxSide: smallArtwork.side, maxBytes: smallArtwork.bytes) else { return nil }
        return Artwork(large: large?.data ?? small.data, small: small.data)
    }

    /// `image` as a JPEG of at most `maxBytes`, as large and sharp as fits: the
    /// largest side up to `maxSide`, scaled down until it fits. Nil if not even
    /// the smallest fits. Any thread.
    static func jpeg(_ image: NSImage, maxSide: Int, maxBytes: Int) -> Data? {
        var rect = CGRect(x: 0, y: 0, width: maxSide, height: maxSide)
        guard let cgImage = image.cgImage(forProposedRect: &rect, context: nil, hints: nil) else { return nil }
        return jpeg(cgImage, maxSide: maxSide, maxBytes: maxBytes)
    }

    static func jpeg(_ image: CGImage, maxSide: Int, maxBytes: Int) -> Data? {
        fit(image, maxSide: maxSide, maxBytes: maxBytes)?.data
    }

    /// The JPEG and its longest side in pixels.
    private static func fit(_ image: CGImage, maxSide: Int, maxBytes: Int) -> (data: Data, side: Int)? {
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
                if data.count <= maxBytes { return (data, max(scaled.width, scaled.height)) }
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
