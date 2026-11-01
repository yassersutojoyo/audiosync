import Foundation

/// The debug log: a plain-text record of what the app did, for whoever debugs
/// it on a real Mac (`docs/DEVELOPMENT.md`).
///
/// It is off unless the environment sets `AUDIOSYNC_DEBUG` to exactly `1`, the
/// same switch the Windows app reads. Then `beginLaunchLog()` empties
/// `/tmp/audiosync-debug.log` at launch and every `log` call appends one line,
/// starting with the time in UTC to the millisecond
/// (`2026-10-18T17:52:13.456Z`), the format of the Windows app's log, so logs
/// from both can be lined up. The file stops growing at
/// `DebugLogFile.standardLimit`, with one last line that says so.
///
/// A message is built only while the log is on: `log` takes an autoclosure, so
/// nothing written inside a message runs in a normal launch (`Tests/run-tests.sh`
/// rejects messages that perform work). Logging never does file work on the
/// caller's thread: the caller builds the message and stamps the time, and one
/// serial queue (`DebugLog`) writes the lines in the order they were handed
/// over. Building a message still allocates, so audio callbacks must not log
/// at all (spec S126, S131); they count events in atomics, and control threads
/// report the counts.
enum DebugTrace {
    /// The environment variable that turns the log on when it is exactly `1`.
    static let environmentKey = "AUDIOSYNC_DEBUG"

    /// Where the log is written.
    static let path = "/tmp/audiosync-debug.log"

    /// True when this run writes the debug log (S170). The core reads the same
    /// variable to turn on the AirPlay sender's trace lines.
    static let enabled = isEnabled(environment: ProcessInfo.processInfo.environment)

    /// The log while it is on; nil otherwise, so a normal launch never touches
    /// the file system for it.
    private static let sink: DebugLog? = enabled ? openSink() : nil

    /// Whether `environment` turns the log on: `AUDIOSYNC_DEBUG` set to exactly
    /// `1`. Any other value, an empty one included, leaves it off.
    static func isEnabled(environment: [String: String]) -> Bool {
        environment[environmentKey] == "1"
    }

    /// Starts this launch's log: empties the file, then writes a first line
    /// with the app and macOS versions and the Mac's local time, so the UTC
    /// times that follow can be matched to the local clock.
    static func beginLaunchLog() {
        sink?.reset(firstLine: launchSummary())
    }

    /// Queues `message` as one line. The message is built, on the caller's
    /// thread, only while the log is on. Never call this from an audio callback.
    static func log(_ message: @autoclosure () -> String) {
        guard let sink else { return }
        sink.log(message())
    }

    /// Waits until every line queued so far is in the file.
    static func flush() {
        sink?.flush()
    }

    /// The log for this run. Lines still queued when the process exits
    /// normally are written first.
    private static func openSink() -> DebugLog {
        let log = DebugLog(file: DebugLogFile(path: path))
        _ = atexit { DebugTrace.flush() }
        return log
    }

    /// The first line of a launch's log.
    static func launchSummary(bundle: Bundle = .main, now: Date = Date()) -> String {
        let info = bundle.infoDictionary ?? [:]
        let version = info["CFBundleShortVersionString"] as? String ?? "unknown"
        let build = info["CFBundleVersion"] as? String ?? "unknown"
        let local = DateFormatter()
        local.locale = Locale(identifier: "en_US_POSIX")
        local.dateFormat = "yyyy-MM-dd HH:mm:ss ZZZZZ"
        return "[AudioSync] debug log: AudioSync \(version) (\(build)), "
            + "macOS \(ProcessInfo.processInfo.operatingSystemVersionString), "
            + "local time \(local.string(from: now))"
    }
}

/// A debug launch that switches no speaker on by itself and starts with every
/// speaker off, so a soak or a test run never reaches the household's
/// speakers. Nothing is saved: the next launch starts from the settings as
/// they were.
enum LaunchAutoConnect {
    /// Set to exactly `1`, next to `AUDIOSYNC_DEBUG=1`, to skip auto-connect.
    static let environmentKey = "AUDIOSYNC_NO_AUTO_CONNECT"

    /// True when this run skips auto-connect.
    static let skipped = skips(environment: ProcessInfo.processInfo.environment)

    /// Whether `environment` skips auto-connect: both this switch and the
    /// debug log's set to exactly `1`; without the debug log it is ignored.
    static func skips(environment: [String: String]) -> Bool {
        DebugTrace.isEnabled(environment: environment) && environment[environmentKey] == "1"
    }
}

/// Hands finished lines to one serial queue that does all the file work, so a
/// thread that logs never takes the file's lock or waits on the disk. Each
/// line keeps the time it was handed over, and lines land in that order.
final class DebugLog: @unchecked Sendable {
    let file: DebugLogFile
    private let queue: DispatchQueue
    private let clock: () -> Date

    init(file: DebugLogFile,
         queue: DispatchQueue = DispatchQueue(label: "audiosync.debug-log", qos: .utility),
         clock: @escaping () -> Date = Date.init) {
        self.file = file
        self.queue = queue
        self.clock = clock
    }

    /// Queues emptying the file, then `firstLine` as its first line.
    func reset(firstLine: String) {
        let now = clock()
        let file = self.file
        queue.async {
            file.empty()
            file.append(firstLine, at: now)
        }
    }

    /// Queues `message` as one line stamped with the current time.
    func log(_ message: String) {
        let now = clock()
        let file = self.file
        queue.async { file.append(message, at: now) }
    }

    /// Waits until every line queued so far is in the file. Not for the main
    /// thread's normal work; for tests and the exit handler.
    func flush() {
        queue.sync {}
    }
}

/// One append-only, size-capped log file that any thread may write to. Lines
/// land in the order their calls take the lock, each through direct `write`
/// calls with no buffering, so the file is complete up to the last line even
/// when the app is killed. Its calls do the file work on the calling thread;
/// the app reaches it only through `DebugLog`'s queue.
///
/// The path is opened without following a symbolic link and without waiting
/// (a FIFO planted there can't block the open), and is used only when it is a
/// regular file owned by this user with no other hard link, so nothing planted
/// at a shared path such as `/tmp` can redirect or empty another file. Only
/// then is it emptied (on `empty()`) and made readable by its owner only. When
/// the file can't be used, logging writes nothing.
final class DebugLogFile: @unchecked Sendable {
    /// The most the log grows to (50 MB, like the Windows app's), so a Mac that
    /// streams for days doesn't fill its disk.
    static let standardLimit = 50 * 1024 * 1024

    let filePath: String
    let limit: Int

    private let lock = NSLock()
    private let clock: () -> Date
    private let stopMessage: String
    private let stopLength: Int
    // Guarded by `lock`.
    private var descriptor: Int32 = -1
    private var size = 0
    private var reachedLimit = false
    private var unusable = false

    init(path: String, limit: Int = DebugLogFile.standardLimit, clock: @escaping () -> Date = Date.init) {
        filePath = path
        self.limit = limit
        self.clock = clock
        let stop = "[AudioSync] debug log reached \(limit / (1024 * 1024)) MB; "
            + "nothing more is written until the next launch"
        stopMessage = stop
        stopLength = DebugLogFile.line(stop, at: Date(timeIntervalSince1970: 0)).utf8.count
    }

    deinit {
        if descriptor >= 0 { close(descriptor) }
    }

    /// Empties the file, creating it when missing, and starts counting toward
    /// the limit again.
    func empty() {
        lock.lock()
        defer { lock.unlock() }
        closeFile()
        unusable = false
        reachedLimit = false
        openFile(truncating: true)
    }

    /// Appends `message` as one line stamped with the current time.
    func append(_ message: String) {
        append(message, at: clock())
    }

    /// Appends `message` as one line stamped with `date`. The file never passes
    /// the limit: a line that would leave no room for the line saying the log
    /// stopped is replaced by it, and later messages are dropped.
    func append(_ message: String, at date: Date) {
        lock.lock()
        defer { lock.unlock() }
        guard !reachedLimit else { return }
        if descriptor < 0 {
            guard !unusable else { return }
            openFile(truncating: false)
            guard descriptor >= 0 else { return }
        }
        var line = Self.line(message, at: date)
        if size + line.utf8.count + stopLength > limit {
            reachedLimit = true
            guard size + stopLength <= limit else { return }
            line = Self.line(stopMessage, at: date)
        }
        writeAll(line)
    }

    /// `message` as a log line: the UTC time to the millisecond, a space, the
    /// message and a line break.
    static func line(_ message: String, at date: Date) -> String {
        timestamp(date) + " " + escaped(message) + "\n"
    }

    /// What starts each line of a message after its first, in place of a stamp.
    static let continuation = "    | "

    /// `message` with each line after the first marked as a continuation and
    /// every other control character escaped (`\r`, `\x1b`), so text from a
    /// device can't pass for an entry of its own.
    static func escaped(_ message: String) -> String {
        let isControl = { (scalar: Unicode.Scalar) in scalar.properties.generalCategory == .control }
        guard message.unicodeScalars.contains(where: isControl) else { return message }
        var out = ""
        for scalar in message.unicodeScalars {
            switch scalar {
            case "\n":
                out += "\n"
                out += continuation
            case "\r": out += "\\r"
            case "\t": out += "\\t"
            case _ where isControl(scalar):
                out += scalar.value < 0x10 ? "\\x0" : "\\x"
                out += String(scalar.value, radix: 16)
            default: out.unicodeScalars.append(scalar)
            }
        }
        return out
    }

    /// `date` in UTC as `yyyy-MM-ddTHH:mm:ss.SSSZ`, rounded down to the
    /// millisecond.
    static func timestamp(_ date: Date) -> String {
        let milliseconds = Int64((date.timeIntervalSince1970 * 1000).rounded(.down))
        let fraction = ((milliseconds % 1000) + 1000) % 1000   // 0…999, also before 1970
        var seconds = time_t((milliseconds - fraction) / 1000)
        var parts = tm()
        gmtime_r(&seconds, &parts)
        return padded(Int(parts.tm_year) + 1900, 4) + "-" + padded(Int(parts.tm_mon) + 1, 2) + "-"
            + padded(Int(parts.tm_mday), 2) + "T" + padded(Int(parts.tm_hour), 2) + ":"
            + padded(Int(parts.tm_min), 2) + ":" + padded(Int(parts.tm_sec), 2) + "."
            + padded(Int(fraction), 3) + "Z"
    }

    /// `value` in decimal with leading zeros up to `width` digits.
    private static func padded(_ value: Int, _ width: Int) -> String {
        let text = String(value)
        return text.count >= width ? text : String(repeating: "0", count: width - text.count) + text
    }

    // MARK: - File access (the caller holds `lock`)

    private func openFile(truncating: Bool) {
        // No O_TRUNC here: nothing is emptied before the checks below pass.
        // O_NONBLOCK keeps a FIFO at the path from blocking the open; it has
        // no effect on writes to a regular file.
        let flags = O_WRONLY | O_CREAT | O_APPEND | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC
        let fd = open(filePath, flags, mode_t(0o600))
        guard fd >= 0 else {
            unusable = true
            return
        }
        var info = stat()
        guard fstat(fd, &info) == 0, (info.st_mode & S_IFMT) == S_IFREG,
              info.st_uid == getuid(), info.st_nlink == 1,
              !truncating || ftruncate(fd, 0) == 0 else {
            close(fd)
            unusable = true
            return
        }
        _ = fchmod(fd, 0o600)   // an older log may have been readable by everyone
        descriptor = fd
        size = truncating ? 0 : Int(info.st_size)
    }

    private func closeFile() {
        if descriptor >= 0 { close(descriptor) }
        descriptor = -1
        size = 0
    }

    private func writeAll(_ line: String) {
        var text = line
        text.withUTF8 { bytes in
            guard let base = bytes.baseAddress else { return }
            var offset = 0
            while offset < bytes.count {
                let result = write(descriptor, base + offset, bytes.count - offset)
                if result > 0 {
                    offset += result
                } else if result == 0 || errno != EINTR {
                    break   // disk full or the file went away: drop the rest; EINTR tries again
                }
            }
            size += offset
        }
    }
}
