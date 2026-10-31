import AppKit
import Foundation

/// Another live copy of this app, if any. Asked fresh each time because the
/// cached `isTerminated` of an old `NSRunningApplication` can lag behind.
func otherRunningCopy() -> NSRunningApplication? {
    guard let bundleID = Bundle.main.bundleIdentifier else { return nil }
    let ownPID = ProcessInfo.processInfo.processIdentifier
    return NSRunningApplication.runningApplications(withBundleIdentifier: bundleID)
        .first { $0.processIdentifier != ownPID && !$0.isTerminated }
}

/// When the kernel started a process, to the microsecond. Unlike
/// `launchDate` it exists for every process, so two copies see the same value.
func processStart(_ pid: pid_t) -> Date? {
    var info = kinfo_proc()
    var size = MemoryLayout<kinfo_proc>.stride
    var mib: [Int32] = [CTL_KERN, KERN_PROC, KERN_PROC_PID, pid]
    guard sysctl(&mib, UInt32(mib.count), &info, &size, nil, 0) == 0, size > 0 else { return nil }
    let start = info.kp_proc.p_un.__p_starttime   // p_starttime in C
    return Date(timeIntervalSince1970: Double(start.tv_sec) + Double(start.tv_usec) / 1_000_000)
}

/// Flipped by the reply observer; the run loop below polls it.
final class ReplyFlag { var received = false }

/// Asks the running copy to show its window and waits for its answer or for
/// it to go. True when this copy leaves.
func leavesToRunningCopy() -> Bool {
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
    if flag.received || stillThere { return true }
    center.removeObserver(token)
    return false
}
