import AppKit
import ImageIO
import UniformTypeIdentifiers

/// Track info for the AirPlay engine's metadata pipe (`AirPlayEngine.metadataPath`),
/// so AirPlay receivers with a screen show the title, artist, album and artwork
/// instead of the name of the audio pipe.
///
/// The format is the item stream Shairport Sync writes to its metadata pipe, as
/// its documentation describes it. One item per field:
///
///     <item><type>636f7265</type><code>6d696e6d</code><length>5</length>
///     <data encoding="base64">
///     SGVsbG8=</data></item>
///
/// The type and code are four ASCII letters as eight hex digits, the length is
/// the number of bytes the base64 data decodes to. Text fields are DAAP items of
/// type 'core' (minm title, asar artist, asal album); artwork is 'ssnc' 'PICT'.
///
/// What the bundled engine does with the stream (OwnTone 29.2's pipe input, read
/// to learn its behavior; none of its code is used here):
/// - It reads the pipe only while it plays the audio pipe, and cuts the stream
///   at every `</item>`. Each piece must be one well-formed item with valid
///   base64; a piece that isn't makes it stop reading until playback starts
///   again. So messages go into the pipe whole (see `MetadataPipeWriter`).
/// - It uses minm, asar, asal, asgn, prgr, pvol, PICT and pfls and skips any
///   other code, so the 'mdst'/'mden' markers that frame a set aren't needed.
/// - It skips an item with no data, and a field it isn't sent keeps its last
///   value: every text message carries all three fields, an empty one as a
///   space.
/// - PICT must be a JPEG or PNG of at most 1 MB. Artwork can be replaced but
///   not cleared.
enum PipeMetadata {

    struct Fields: Equatable {
        var title: String
        var artist: String
        var album: String
    }

    /// The most one message may hold: what an empty pipe on macOS takes in one
    /// write (its buffer is 8 KB; a larger write is cut short). Tests/fifotest.swift
    /// checks it.
    static let messageLimit = 8192
    /// A longer text field is cut (between characters), so the three always fit
    /// one message.
    static let fieldLimit = 512
    /// Stands in for an empty field, which the engine would skip.
    static let blank = " "

    /// One item. `type` and `code` are four ASCII letters each.
    static func item(type: String, code: String, data: Data) -> Data {
        var s = "<item><type>\(hex(type))</type><code>\(hex(code))</code><length>\(data.count)</length>"
        if !data.isEmpty {
            s += "\n<data encoding=\"base64\">\n" + data.base64EncodedString() + "</data>"
        }
        s += "</item>\n"
        return Data(s.utf8)
    }

    /// Four ASCII letters as eight lowercase hex digits ("core" → "636f7265").
    static func hex(_ code: String) -> String {
        code.utf8.map { String(format: "%02x", $0) }.joined()
    }

    /// Title, artist and album as one message.
    static func text(_ fields: Fields) -> Data {
        item(type: "core", code: "minm", data: field(fields.title))
            + item(type: "core", code: "asar", data: field(fields.artist))
            + item(type: "core", code: "asal", data: field(fields.album))
    }

    /// A JPEG or PNG as one message.
    static func picture(_ image: Data) -> Data {
        item(type: "ssnc", code: "PICT", data: image)
    }

    /// The bytes sent for one text field: trimmed, at most `fieldLimit` bytes
    /// of whole characters, `blank` when nothing is left.
    static func field(_ value: String) -> Data {
        var out = ""
        var bytes = 0
        for character in value.trimmingCharacters(in: .whitespacesAndNewlines) {
            let size = character.utf8.count
            if bytes + size > fieldLimit { break }
            out.append(character)
            bytes += size
        }
        return Data((out.isEmpty ? blank : out).utf8)
    }

    /// The largest image whose picture message still fits `messageLimit`.
    static let pictureLimit: Int = {
        var bytes = messageLimit * 3 / 4
        while bytes > 0 && picture(Data(count: bytes)).count > messageLimit { bytes -= 1 }
        return bytes
    }()

    // MARK: - Artwork

    /// At most this many pixels a side: what the engine asks for when it sends
    /// artwork on to a speaker.
    static let artworkSide = 600
    private static let smallestSide = 64
    private static let qualities = [0.65, 0.5, 0.4]

    /// `image` as a JPEG of at most `maxBytes`, as large and sharp as fits: the
    /// largest side up to `maxSide`, scaled down until it fits. Nil if not even
    /// the smallest fits. Any thread.
    static func jpeg(_ image: NSImage, maxSide: Int = artworkSide, maxBytes: Int = pictureLimit) -> Data? {
        var rect = CGRect(x: 0, y: 0, width: maxSide, height: maxSide)
        guard let cgImage = image.cgImage(forProposedRect: &rect, context: nil, hints: nil) else { return nil }
        return jpeg(cgImage, maxSide: maxSide, maxBytes: maxBytes)
    }

    static func jpeg(_ image: CGImage, maxSide: Int = artworkSide, maxBytes: Int = pictureLimit) -> Data? {
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

/// Writes `PipeMetadata` messages into the metadata pipe without ever blocking
/// its caller: all work happens on one serial utility queue, the descriptors
/// are non-blocking, and nothing waits for the engine. Its write end also keeps
/// the pipe from reading as end-of-file, which the engine would re-open in a
/// loop.
///
/// Every message reaches the engine whole, because one broken item makes it
/// stop reading (see `PipeMetadata`):
/// - A message goes out in one write, only into an empty pipe, and is never
///   larger than `PipeMetadata.messageLimit`, all of which an empty pipe takes.
///   So at most one message waits in the pipe while the engine isn't reading.
/// - Newer text, or a newer picture, replaces the one still waiting here: a
///   stuck pipe holds nothing older than that one message, and the latest goes
///   out once the engine has read it. Retries back off and end after
///   `retryWindow`; the next message starts them again.
/// - Should a write still take only part of a message, the rest goes first.
/// - The pipe is only ever measured here, never read: the engine also stops
///   reading when its read finds the data gone.
final class MetadataPipeWriter {

    /// How long a pipe nobody reads is retried before the writer waits for the
    /// next message.
    let retryWindow: TimeInterval
    private let queue = DispatchQueue(label: "io.github.yassersutojoyo.audiosync.metadata", qos: .utility)

    // Queue only.
    /// Read-only end, never read from: FIONREAD on it tells what the engine
    /// hasn't read yet (on a write or read-write end it always says 0).
    private var reader: Int32 = -1
    private var writer: Int32 = -1
    private var text: Data?
    private var picture: Data?
    /// The unwritten end of a message the pipe took only part of.
    private var rest = Data()
    /// Bumped on every open and close, so a retry scheduled before is dropped.
    private var generation = 0
    private var retryScheduled = false
    private var retryDelay = MetadataPipeWriter.firstRetry
    private var retryUntil = Date.distantPast
    private static let firstRetry: TimeInterval = 0.05
    private static let slowestRetry: TimeInterval = 1
    /// FIONREAD, `_IOR('f', 127, int)`, which Swift doesn't import.
    private static let unreadRequest: UInt = 0x4004_667F

    init(retryWindow: TimeInterval = 30) {
        self.retryWindow = retryWindow
    }

    /// Opens the FIFO at `path`, closing any pipe opened before. Returns 0, or
    /// the errno of what failed. Waits for the queue, which never blocks.
    func open(_ path: String) -> Int32 {
        queue.sync {
            reset()
            // Read end first: a non-blocking write end can't open without one.
            let reader = Darwin.open(path, O_RDONLY | O_NONBLOCK | O_CLOEXEC)
            guard reader >= 0 else { return errno }
            var info = stat()
            guard fstat(reader, &info) == 0, info.st_mode & S_IFMT == S_IFIFO else {
                Darwin.close(reader)
                return EINVAL
            }
            let writer = Darwin.open(path, O_WRONLY | O_NONBLOCK | O_CLOEXEC)
            guard writer >= 0 else {
                let error = errno
                Darwin.close(reader)
                return error
            }
            self.reader = reader
            self.writer = writer
            return 0
        }
    }

    /// Closes the pipe and drops whatever is waiting. Waits for the queue.
    func close() {
        queue.sync { reset() }
    }

    /// Title, artist and album (`PipeMetadata.text`). Replaces text not written
    /// yet; ignored while the pipe is closed.
    func send(text message: Data) {
        queue.async {
            guard self.writer >= 0 else { return }
            self.text = message
            self.restartRetries()
            self.pump()
        }
    }

    /// Artwork, scaled and encoded on the writer's queue. Replaces a picture
    /// not written yet; ignored while the pipe is closed.
    func send(picture image: NSImage) {
        queue.async {
            guard self.writer >= 0 else { return }
            guard let jpeg = PipeMetadata.jpeg(image) else {
                DBG.log("[AudioSync] artwork could not be made small enough for the metadata pipe")
                return
            }
            self.picture = PipeMetadata.picture(jpeg)
            self.restartRetries()
            self.pump()
        }
    }

    /// Returns once everything sent before has been handled (tests).
    func waitUntilIdle() { queue.sync {} }

    // MARK: - Queue only

    private func reset() {
        if reader >= 0 { Darwin.close(reader) }
        if writer >= 0 { Darwin.close(writer) }
        reader = -1
        writer = -1
        text = nil
        picture = nil
        rest = Data()
        generation += 1
        retryScheduled = false
    }

    private func pump() {
        guard writer >= 0 else { return }
        if !rest.isEmpty {
            guard finish() else { scheduleRetry(); return }
        }
        while text != nil || picture != nil {
            // One message at a time: the engine must have read the last one.
            guard unread() == 0 else { scheduleRetry(); return }
            let (message, takesText, takesPicture) = nextMessage()
            let written = write(message)
            let error = errno
            if written < 0 && Self.isBusy(error) { scheduleRetry(); return }
            if takesText { text = nil }
            if takesPicture { picture = nil }
            if written < 0 {
                DBG.log("[AudioSync] metadata pipe write failed (errno \(error)); message dropped")
            } else if written < message.count {
                rest = message.subdata(in: message.startIndex + written ..< message.endIndex)
                guard finish() else { scheduleRetry(); return }
            }
        }
    }

    /// Writes what is left of a message cut short. True once nothing is left.
    private func finish() -> Bool {
        let written = write(rest)
        let error = errno
        if written > 0 {
            rest = rest.subdata(in: rest.startIndex + written ..< rest.endIndex)
        } else if written < 0 && !Self.isBusy(error) {
            DBG.log("[AudioSync] metadata pipe write failed (errno \(error)); message dropped")
            rest = Data()
        }
        return rest.isEmpty
    }

    /// Text first; text and picture in one write when both fit.
    private func nextMessage() -> (Data, Bool, Bool) {
        if let text, let picture, text.count + picture.count <= PipeMetadata.messageLimit {
            return (text + picture, true, true)
        }
        if let text { return (text, true, false) }
        return (picture ?? Data(), false, true)
    }

    private func write(_ data: Data) -> Int {
        data.withUnsafeBytes { Darwin.write(writer, $0.baseAddress, $0.count) }
    }

    /// Bytes in the pipe the engine hasn't read.
    private func unread() -> Int {
        var count: Int32 = 0
        let result = withUnsafeMutablePointer(to: &count) { ioctl(reader, Self.unreadRequest, $0) }
        return result == 0 ? Int(count) : 0
    }

    private static func isBusy(_ error: Int32) -> Bool {
        error == EAGAIN || error == EINTR
    }

    private func restartRetries() {
        retryDelay = Self.firstRetry
        retryUntil = Date().addingTimeInterval(retryWindow)
    }

    private func scheduleRetry() {
        guard !retryScheduled, Date() < retryUntil else { return }
        retryScheduled = true
        let generation = generation
        queue.asyncAfter(deadline: .now() + retryDelay) {
            guard self.generation == generation else { return }
            self.retryScheduled = false
            self.pump()
        }
        retryDelay = min(retryDelay * 2, Self.slowestRetry)
    }
}
