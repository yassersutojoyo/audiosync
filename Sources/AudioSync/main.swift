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

// The exit signals run the quit steps a Quit runs, fades included, and then
// exit with 143 or 130 (S111). They are handled off the main queue, which runs
// those steps but may be held by something else, so the bound on the wait for
// them (`SignalQuit`) still ends the app. One serial queue for all of them, so
// the app exits exactly once, however many signals arrive.
let exitSignalQueue = DispatchQueue(label: "audiosync.exit-signals", qos: .userInitiated)
let signalQuit = SignalQuit(queue: exitSignalQueue, quitSteps: { done in
    // Where Quit runs them. A modal alert's run loop serves the main queue too,
    // so an open alert doesn't hold them back.
    DispatchQueue.main.async {
        MainActor.assumeIsolated { launchDelegate.runQuitSteps() }
        done()
    }
}, exitProcess: { status in
    // Off the main thread, possibly while Core Audio or the core is still
    // tearing down: flush the log ourselves and skip the exit handlers and
    // static destructors, which would race that teardown.
    DebugTrace.flush()
    _exit(status)
})
let exitSignalSources = AppLifecycle.exitSignals.map { number -> DispatchSourceSignal in
    signal(number, SIG_IGN)   // the default action would end the process before the source sees it
    let source = DispatchSource.makeSignalSource(signal: number, queue: exitSignalQueue)
    source.setEventHandler { @Sendable in
        // A launch arriving during the shutdown should not be told we're here.
        AppDelegate.exiting.store(true, ordering: .relaxed)
        signalQuit.handle(signal: number)
    }
    source.resume()
    return source
}

// `exitSignalSources` is a top-level constant, so the sources live as long as the process.
launchApp.run()
