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
    /// it answers nil (no input device captured), which hides it.
    func follow(_ peak: @escaping () -> Float?) {
        timer?.invalidate()
        timer = nil
        guard update(peak: peak()) else { return }
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
