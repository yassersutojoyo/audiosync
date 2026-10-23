import AppKit

// Hide Speaker and Hidden Speakers.

extension AppDelegate {

    /// Hide Speaker: a speaker that is on goes off first, the way its own
    /// switch turns it off (streaming stops with the last one, a Cast session
    /// ends, an AirPlay pairing is cancelled), then it leaves every list.
    /// Its volume, delay, name, groups and auto-connect mark stay saved.
    func hideSpeaker(_ key: String, name: String, kind: String) {
        if let uid = localReference, key == SpeakerKey.make(id: uid, airplay: false) { return }
        let rows = appModel.airplay + appModel.cast + self.appModel.local
        let speaker = rows.first { HiddenSpeakers.key(id: $0.id, tag: $0.tag) == key }
        let wasOn = speaker?.enabled == true
        if let speaker {
            switch speaker.tag {
            case .airplay:
                if wasOn { setNativeAirPlayEnabled(speaker.id, false) } else { endNativeAttempt(speaker.id) }
            case .cast:
                if wasOn { setCastEnabled(speaker.id, false) }
            default:
                if wasOn { setLocalEnabled(speaker.id, false) }
            }
        }
        clearNotices([key])   // its row is gone, so is what it said
        hiddenSpeakers.hide(key: key, name: name, kind: kind)
        DebugTrace.log("[AudioSync] speaker hidden: \(key), was \(wasOn ? "on" : "off")")
        pushModel()
    }

    /// A Hidden Speakers entry: the speaker's row comes back, off, with its
    /// saved values. Auto-connect waits for its next appearance.
    func unhideSpeaker(_ key: String) {
        guard hiddenSpeakers.show(key) != nil else { return }
        DebugTrace.log("[AudioSync] speaker shown again: \(key)")
        pushModel()
    }

    /// An entry of the Speakers menu's Hidden Speakers; each carries its key.
    @objc func unhideFromMenu(_ sender: Any?) {
        guard let key = (sender as? NSMenuItem)?.representedObject as? String,
              let entry = appModel.hidden.menu.first(where: { $0.key == key }) else { return }
        appModel.unhide(entry)
    }

    /// The Hidden Speakers menu, and which sections have a present speaker
    /// hidden: `local` and `network` are the hidden rows of each kind.
    func hiddenSnapshot(local: [AppModel.Speaker], network: [AppModel.Speaker]) -> AppModel.HiddenState {
        var present: [String: (name: String, kind: String)] = [:]
        for s in local + network {
            present[HiddenSpeakers.key(id: s.id, tag: s.tag)] = (s.name, text.speakerTag(s.tag, airPlay2: s.airPlay2))
        }
        let split = SpeakerLayout.split(local: local, airplay: network, tag: \.tag, name: \.name)
        return AppModel.HiddenState(menu: hiddenSpeakers.menu(present: present, text: text),
                                    inThisMac: !split.thisMac.isEmpty, inConnected: !split.connected.isEmpty)
    }
}

/// Fills the Speakers menu's Hidden Speakers submenu each time it opens.
@MainActor
final class HiddenSpeakersMenu: NSObject, NSMenuDelegate {
    static let shared = HiddenSpeakersMenu()

    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        guard let app = NSApp.delegate as? AppDelegate else { return }
        let entries = app.appModel.hidden.menu
        guard !entries.isEmpty else {
            // No action, so the menu shows it disabled.
            menu.addItem(NSMenuItem(title: app.text.noHiddenSpeakers, action: nil, keyEquivalent: ""))
            return
        }
        for entry in entries {
            let item = NSMenuItem(title: entry.title, action: #selector(AppDelegate.unhideFromMenu(_:)),
                                  keyEquivalent: "")
            item.representedObject = entry.key
            menu.addItem(item)
        }
    }
}
