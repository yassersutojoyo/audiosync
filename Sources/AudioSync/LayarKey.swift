import SwiftUI

/// A labelled element's bounds for the LAYAR dump in
/// scripts/screenshots/render.swift, keyed by the label the desktop's
/// layar_shots reads for the same element. SwiftUI builds no accessibility
/// tree in an offscreen window, so the dump can't read the labels themselves.
struct LayarMark {
    let key: String
    let anchor: Anchor<CGRect>
}

struct LayarMarks: PreferenceKey {
    static let defaultValue: [LayarMark] = []
    static func reduce(value: inout [LayarMark], nextValue: () -> [LayarMark]) {
        value.append(contentsOf: nextValue())
    }
    /// Only the renderer sets AUDIOSYNC_LAYAR; in the app no mark goes up the tree.
    static let recording = ProcessInfo.processInfo.environment["AUDIOSYNC_LAYAR"] != nil
}

extension View {
    func layarKey(_ key: String) -> some View {
        anchorPreference(key: LayarMarks.self, value: .bounds) { anchor in
            LayarMarks.recording ? [LayarMark(key: key, anchor: anchor)] : []
        }
    }
}
