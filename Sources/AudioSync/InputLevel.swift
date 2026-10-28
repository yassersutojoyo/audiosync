import Combine
import Foundation
import CAudioSyncCore

/// The input level meter under the main window's source picker, while an
/// input device is captured (main thread). Its own object, so the ten updates a second
/// redraw the meter alone and not every view of `AppModel`. The dB scale and
/// the fall-back are the core's (`asc_meter_level`).
final class InputLevel: ObservableObject {
    /// The bar's length, 0 to 1.
    @Published private(set) var level: Float = 0
    @Published private(set) var shown = false
    private var timer: Timer?
    private var peak: (() -> Float?)?
    private var visible = false

    var isPolling: Bool { timer?.isValid == true }

    /// The level as VoiceOver reads it, in steps of 10 percent (the core's
    /// rule, shared with the Windows app).
    var spokenPercent: Int { Int(asc_meter_spoken_percent(level)) }

    /// Moves the meter to `peak`, the largest sample magnitude since the last
    /// update; nil hides it. Returns whether it shows.
    @discardableResult
    func update(peak: Float?) -> Bool {
        guard let peak else {
            if shown { shown = false }
            if level != 0 { level = 0 }
            return false
        }
        let next = asc_meter_level(level, peak)
        if next != level { level = next }
        if !shown { shown = true }
        return true
    }

    /// Updates the meter from `peak` now and every `ASC_METER_POLL_MS` until
    /// it answers nil (no input device captured), which hides it; only while
    /// the main window is on screen.
    func follow(_ peak: @escaping () -> Float?) {
        self.peak = peak
        restartPolling()
    }

    /// Whether the main window, the meter's only place, is on screen.
    func setVisible(_ visible: Bool) {
        self.visible = visible
        restartPolling()
    }

    private func restartPolling() {
        timer?.invalidate()
        timer = nil
        guard visible, let peak, update(peak: peak()) else { return }
        let poll = Timer(timeInterval: Double(ASC_METER_POLL_MS) / 1000, repeats: true) { [weak self] timer in
            guard let self, self.update(peak: peak()) else {
                timer.invalidate()
                return
            }
        }
        RunLoop.main.add(poll, forMode: .common)
        timer = poll
    }
}

/// The note under the source while an input device plays and an AirPlay
/// speaker is on: every speaker then waits the AirPlay delay, which is too
/// late for live sound.
enum LiveInputAirPlayHint {
    /// The AirPlay delay to name, in tenths of a second; nil hides the note.
    static func tenths(source: AudioSource, airPlayOn: Bool, airPlayDelayMs: Double) -> Int? {
        guard case .input = source, airPlayOn else { return nil }
        return Int((max(0, airPlayDelayMs) / 100).rounded())
    }
}
