import AppKit
import Foundation
import Synchronization

// MARK: - One copy at a time

// A copy that defers asks the running one only from `applicationDidFinishLaunching`,
// the first place that can tell a launch at login (S215).
let secondCopy: Bool = {
    guard let other = otherRunningCopy(),
          let ownStart = processStart(ProcessInfo.processInfo.processIdentifier),
          let otherStart = processStart(other.processIdentifier) else { return false }
    return AppLifecycle.defersToOtherCopy(ownPID: ProcessInfo.processInfo.processIdentifier,
                                          ownStart: ownStart,
                                          otherPID: other.processIdentifier,
                                          otherStart: otherStart,
                                          otherFinishedLaunching: other.isFinishedLaunching)
}()

// MARK: - Application

let launchApp = NSApplication.shared
let launchDelegate = AppDelegate()   // NSApplication holds its delegate weakly
launchApp.delegate = launchDelegate
launchDelegate.defersToRunningCopy = secondCopy
launchApp.setActivationPolicy(secondCopy ? .prohibited : .regular)

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
