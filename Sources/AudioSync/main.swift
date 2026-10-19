import AppKit
import Foundation
import Synchronization

// MARK: - One copy at a time

/// Another live copy of this app, if any. Asked fresh each time because the
/// cached `isTerminated` of an old `NSRunningApplication` can lag behind.
func otherRunningCopy() -> NSRunningApplication? {
    guard let bundleID = Bundle.main.bundleIdentifier else { return nil }
    let ownPID = ProcessInfo.processInfo.processIdentifier
    return NSRunningApplication.runningApplications(withBundleIdentifier: bundleID)
        .first { $0.processIdentifier != ownPID && !$0.isTerminated }
}

/// Flipped by the reply observer; the run loop below polls it.
final class ReplyFlag { var received = false }

if otherRunningCopy() != nil {
    let center = DistributedNotificationCenter.default()
    let flag = ReplyFlag()
    let token = center.addObserver(forName: AppDelegate.showReply, object: nil, queue: nil) { _ in
        flag.received = true
    }
    center.postNotificationName(AppDelegate.showRequest, object: nil, userInfo: nil, deliverImmediately: true)

    // A copy that is quitting stops answering but may need its whole shutdown
    // (the AirPlay sender's teardown takes up to about 2 s) to disappear, so
    // wait well past that before giving up.
    let limit = Date().addingTimeInterval(5.5)
    var stillThere = true
    while !flag.received && Date() < limit {
        // With no input source attached the run loop returns at once; pause
        // so the wait never spins the CPU.
        if !RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.05)) {
            Thread.sleep(forTimeInterval: 0.05)
        }
        if otherRunningCopy() == nil {
            stillThere = false
            break
        }
    }
    // The other copy may have gone away during the last tick before the deadline.
    if stillThere && !flag.received && otherRunningCopy() == nil {
        stillThere = false
    }

    // Answered, or alive but silent: that copy owns the speakers, so leave
    // without a word. Only a copy that went away lets this one take over.
    if flag.received || stillThere { exit(0) }
    center.removeObserver(token)
}

// MARK: - Application

let launchApp = NSApplication.shared
let launchDelegate = AppDelegate()   // NSApplication holds its delegate weakly
launchApp.delegate = launchDelegate
launchApp.setActivationPolicy(.regular)

// MARK: - Signals

// Writing to a pipe whose reader is gone should fail with EPIPE where the
// write happened, not end the whole process.
signal(SIGPIPE, SIG_IGN)

// The exit signals are handled off the main queue because a modal alert can
// keep the main queue busy, and the app must still leave at once. One serial
// queue for all of them, so two signals arriving together never run the
// shutdown twice at once.
let exitSignalQueue = DispatchQueue(label: "audiosync.exit-signals", qos: .userInitiated)
let exitSignalSources = AppLifecycle.exitSignals.map { number -> DispatchSourceSignal in
    signal(number, SIG_IGN)   // the default action would end the process before the source sees it
    let source = DispatchSource.makeSignalSource(signal: number, queue: exitSignalQueue)
    source.setEventHandler { @Sendable in
        // A launch arriving during the shutdown should not be told we're here.
        AppDelegate.exiting.store(true, ordering: .relaxed)
        exit(0)
    }
    source.resume()
    return source
}

// `exitSignalSources` is a top-level constant, so the sources live as long as the process.
launchApp.run()
