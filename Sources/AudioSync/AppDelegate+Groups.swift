import AppKit
import ServiceManagement

// Speaker groups, auto-connect and the login item.

extension AppDelegate {

    // MARK: - Groups, auto-connect, login item

    func isAvailable(_ key: String) -> Bool {
        guard let (id, airplay) = SpeakerKey.parse(key) else { return false }
        return airplay ? airplayOrder.contains(id) : localDevices[id] != nil && localOrder.contains(id)
    }

    func isEnabled(_ key: String) -> Bool {
        guard let (id, airplay) = SpeakerKey.parse(key) else { return false }
        return airplay ? enabledAirPlay.contains(id) : enabledLocal.contains(id)
    }

    /// All present members on while streaming → switch the group off; otherwise
    /// switch every present member on (starting streaming if it is off). Absent
    /// members are left alone.
    func toggleGroup(_ id: String) {
        guard let group = groups.first(where: { $0.id == id }) else { return }
        let present = group.members.filter(isAvailable)
        guard !present.isEmpty else { return }
        // While streaming is off, a click starts it, even for a group already chosen.
        let turnOn = SpeakerState(enabled: present.allSatisfy(isEnabled), streaming: engine.running).clickTurnsOn
        // A new attempt for each member this click starts; while streaming is
        // off that is every member, chosen or not.
        if turnOn { clearNotices(present.filter { !engine.running || !isEnabled($0) }) }
        var attempts: [String] = []   // AirPlay members this click turns on
        for key in present {
            guard let (sid, airplay) = SpeakerKey.parse(key) else { continue }
            guard airplay else {
                if turnOn { enabledLocal.insert(sid) } else { enabledLocal.remove(sid) }
                continue
            }
            // Only members this click turns on: one already on may be
            // mid-pairing, and a fresh click would reset that. While
            // streaming is off nothing is pairing, and the click starts all.
            if !turnOn {
                endNativeAttempt(sid)   // a pairing under way stops
            } else if !engine.running || !enabledAirPlay.contains(sid) {
                attempts.append(sid)
            }
            if turnOn { enabledAirPlay.insert(sid) } else { enabledAirPlay.remove(sid) }
        }
        beginNativeAttempts(attempts)   // before the sender's speakers follow
        if turnOn && !engine.running {
            startEngine(origin: .user)
        } else if engine.running {
            applyAirPlaySelection()   // the sender's speakers follow the group's switches
            syncLocalOutputs(origin: .user)
            stopIfIdle()
        }
        pushModel()
    }

    func promptNewGroup() {
        menuBar.close()
        let members = enabledAirPlay.map { SpeakerKey.make(id: $0, airplay: true) }
            + enabledLocal.map { SpeakerKey.make(id: $0, airplay: false) }
        guard !members.isEmpty else {
            showAlert(text.newGroupNeedsSpeakers, style: .informational)
            return
        }
        NSApp.activate(ignoringOtherApps: true)
        let prompt = NSAlert()
        prompt.messageText = text.newGroupTitle
        prompt.informativeText = text.newGroupMessage
        let field = NSTextField(frame: NSRect(x: 0, y: 0, width: 240, height: 24))
        field.placeholderString = text.newGroupPlaceholder
        field.setAccessibilityLabel(text.newGroupFieldLabel)
        prompt.accessoryView = field
        prompt.addButton(withTitle: text.create)
        prompt.addButton(withTitle: text.cancel)
        prompt.window.initialFirstResponder = field
        guard prompt.runModal() == .alertFirstButtonReturn else { return }
        let name = GroupName.forNewGroup(typed: field.stringValue, existing: groups.map(\.name),
                                         numbered: text.numberedGroup)
        groups.append(SpeakerGroup(id: UUID().uuidString, name: name, members: members.sorted()))
        pushModel()
    }

    /// The user chose Rename Group…; a blank name keeps the current one.
    func promptRenameGroup(_ id: String) {
        guard let current = groups.first(where: { $0.id == id })?.name else { return }
        menuBar.close()
        NSApp.activate(ignoringOtherApps: true)
        let prompt = NSAlert()
        prompt.messageText = text.renameGroupTitle
        prompt.informativeText = text.renameGroupMessage(current)
        let field = NSTextField(frame: NSRect(x: 0, y: 0, width: 240, height: 24))
        field.stringValue = current
        field.placeholderString = text.newGroupPlaceholder
        field.setAccessibilityLabel(text.renameGroupTitle)
        prompt.accessoryView = field
        prompt.addButton(withTitle: text.rename)
        prompt.addButton(withTitle: text.cancel)
        prompt.window.initialFirstResponder = field
        guard prompt.runModal() == .alertFirstButtonReturn,
              let index = groups.firstIndex(where: { $0.id == id }) else { return }
        let name = GroupName.afterRename(typed: field.stringValue, current: current)
        guard name != groups[index].name else { return }
        groups[index].name = name   // saved by `groups`' didSet, like a new group
        pushModel()
    }

    /// Speakers marked auto-connect that just appeared (or were there at launch):
    /// switch them on, then start streaming once for all of them. Speakers still
    /// on from the last session only need the stream started.
    func autoConnect(_ keys: [String]) {
        let hits = keys.filter { autoConnect.contains($0) }
        guard !hits.isEmpty else { return }
        DBG.log("[AudioSync] auto-connect \(hits)")
        for key in hits where !isEnabled(key) {
            guard let (id, airplay) = SpeakerKey.parse(key) else { continue }
            // Switched on automatically: an AirPlay speaker's code request becomes a notice.
            if airplay { enabledAirPlay.insert(id) } else { enabledLocal.insert(id) }
        }
        if !engine.running, !userStopped, !autoStartFailed {
            startEngine(origin: .automatic)
        } else if engine.running {
            applyAirPlaySelection()   // the speakers just switched on join the session under way
            syncLocalOutputs(origin: .automatic)
        }
        pushModel()
    }

    func setLaunchAtLogin(_ on: Bool) {
        let service = SMAppService.mainApp
        clearNotices([Notice.login])   // a new attempt
        do {
            if on { try service.register() } else { try service.unregister() }
        } catch {
            DBG.log("[AudioSync] login item: \(error)")
            if on { notify(text.launchAtLoginFailed, origin: .user, subject: Notice.login) }
        }
        if on && service.status == .requiresApproval {
            notify(text.launchAtLoginNeedsApproval, origin: .user, subject: Notice.login, style: .informational)
            SMAppService.openSystemSettingsLoginItems()
        }
        launchAtLogin = service.status == .enabled
        pushModel()
    }
}
