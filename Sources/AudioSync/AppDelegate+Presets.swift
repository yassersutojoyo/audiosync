import AppKit
import SwiftUI

// Speaker presets in the windows, the menus and the panel (S259 to S269).

extension AppDelegate {

    // MARK: - What plays now

    /// The speakers on in the windows' order, the zones, the master and the
    /// ten gains (S251, S253).
    func presetScene() -> PresetScene {
        let layout = appModel.layout
        let on = (layout.thisMac + layout.connected).filter(\.enabled).map { speaker in
            PresetScene.Speaker(key: speaker.key, volume: speaker.cast?.volumeFixed == true ? nil : speaker.volume)
        }
        return PresetScene(on: on, zones: zones, master: masterVolume,
                           gains: equalizerModel?.settings.gains ?? Equalizer.Settings().gains)
    }

    /// The rows the windows show, worked out again on every push (S264).
    func presetSnapshot() -> [PresetRow] {
        PresetCore.rows(presets, presetScene())
    }

    // MARK: - Recall

    /// Every recall's one entry point (S265): the core's plan carried out as
    /// a zone click carries out its own, then the master, the zone volumes,
    /// the own volumes and the ten gains. It opens, raises and focuses
    /// nothing (S266), and no path raises an alert. The members in `keepOff`
    /// (a phone's recall: they ask for a code) count as not found and get the
    /// pair-first notice instead (S257).
    func recallPreset(_ id: String, from source: PresetRecallSource, keepingOff keepOff: [String] = []) {
        guard let preset = presets.first(where: { $0.id == id }) else { return }
        let states = preset.members.map { keepOff.contains($0.key) ? nil : memberState($0.key) }
        postPairFirst(preset.members.map(\.key).filter { keepOff.contains($0) })
        let facts = zip(preset.members, states).map { member, state -> PresetRecall.Member in
            guard let state else { return PresetRecall.Member(kind: .local, found: false) }
            // A member a target holds leaves the rows but is still there (S247).
            return PresetRecall.Member(kind: state.facts.kind, found: state.found || state.target != nil,
                                       hidden: isHidden(member.key), on: state.facts.on, busy: state.facts.busy,
                                       needsAirPlay2: state.facts.needsAirPlay2, inTarget: state.facts.inTarget,
                                       fixedVolume: hasFixedVolume(state), hasVolume: member.volume != nil)
        }
        let stored = Set(preset.members.map(\.key))
        let others = speakersOnNow().filter { !stored.contains($0.key) }
        let running = engine.isRunning
        let plan = PresetRecall.plan(facts, others: others.map { _ in true }, streaming: running)
        let leftOff = zip(preset.members, zip(states, plan.steps)).compactMap { member, pair -> PresetNotice.LeftOff? in
            let (state, step) = pair
            guard case .leftOff(let reason) = step, !keepOff.contains(member.key) else { return nil }
            return PresetNotice.LeftOff(name: state.map(memberName) ?? text.unknownDevice, reason: reason,
                                        target: state?.target.map { shownName(airPlay: NativeAirPlayID.app($0)) },
                                        key: member.key)
        }
        DebugTrace.log("[AudioSync] preset \"\(preset.name)\" recalled (\(plan.action)), \(leftOff.count) left off")
        guard plan.action != .nothing else {
            showPresetNotice(preset, leftOff: leftOff, nothingChanged: true)
            pushModel()
            return
        }

        setMasterVolume(preset.master)
        var zoned = zones
        for stored in preset.zones {
            if let index = zoned.firstIndex(where: { $0.id == stored.id }) { zoned[index].volume = stored.volume }
        }
        if zoned != zones { zones = zoned }
        for (index, member) in preset.members.enumerated() where plan.setVolume[index] {
            guard let state = states[index], let volume = member.volume else { continue }
            switch state.kind {
            case .local: setLocalVolume(state.appID, volume)
            case .airplay: setNativeAirPlayVolume(state.appID, volume)
            case .cast: continue   // once its session is live, below
            }
        }
        if var equalizer = equalizerModel?.settings, equalizer.gains != preset.gains {
            equalizer.gains = preset.gains   // the preamp and the switch stay (S252)
            equalizerModel?.set(equalizer)
        }

        var attempts: [String] = []
        // Off first, so the preset's Cast speaker isn't refused for another one.
        for (other, off) in zip(others, plan.switchOff) where off {
            switchZoneMember(other, on: false, attempts: &attempts)
        }
        for (state, step) in zip(states, plan.steps) {
            guard let state, step == .on || step == .waiting else { continue }
            if !running || !state.facts.on { switchZoneMember(state, on: true, attempts: &attempts) }
        }
        if source.beginsAttempts { beginNativeAttempts(attempts) }
        if plan.action == .start {
            startEngine(origin: source.origin)
        } else if engine.isRunning {
            applyAirPlaySelection()
            syncLocalOutputs(origin: source.origin)
        }
        syncCast()
        for (index, member) in preset.members.enumerated() where plan.setVolume[index] {
            guard let state = states[index], state.kind == .cast, let volume = member.volume else { continue }
            setCastVolume(state.appID, volume)
        }
        stopIfIdle()
        retimeLocalOutputs()
        showPresetNotice(preset, leftOff: leftOff, nothingChanged: false)
        pushModel()
    }

    /// The speakers switched on now, as zone members.
    private func speakersOnNow() -> [ZoneMemberState] {
        let keys = enabledLocal.sorted().map { SpeakerKey.make(id: $0, kind: .local) }
            + enabledAirPlay.sorted().map { SpeakerKey.make(id: $0, kind: .airplay) }
            + (castModel.enabled.map { [CastID.key($0)] } ?? [])
        return keys.compactMap(memberState)
    }

    private func hasFixedVolume(_ state: ZoneMemberState) -> Bool {
        guard state.kind == .cast, let device = CastID.device(state.appID) else { return false }
        return castModel.volumes[device]?.fixed == true
    }

    /// Posts the preset notice, or clears it after a full recall (S265).
    /// Only a notice, never an alert; VoiceOver hears a new one.
    private func showPresetNotice(_ preset: SpeakerPreset, leftOff: [PresetNotice.LeftOff], nothingChanged: Bool) {
        guard let message = PresetNotice.message(preset: preset.name, leftOff: leftOff,
                                                 nothingChanged: nothingChanged, text: text) else {
            clearPresetNotice()
            return
        }
        let repeated = appModel.notices.items.contains { $0.subject == Notice.presets && $0.text == message }
        postNotice(message, subject: Notice.presets, title: text.presetLeftOffTitle)
        presetNotice = PresetNotice.Cause(presetID: preset.id, leftOff: leftOff, nothingChanged: nothingChanged)
        if !repeated { AccessibilityNotification.Announcement(message).post() }
    }

    private func clearPresetNotice() {
        clearNotices([Notice.presets])
        presetNotice = nil
    }

    /// On every push: the notice goes once its cause is gone (S265), and a
    /// notice the user dismissed is forgotten.
    func followPresetNotice(current: String?) {
        guard let cause = presetNotice else { return }
        guard appModel.notices.items.contains(where: { $0.subject == Notice.presets }) else {
            presetNotice = nil
            return
        }
        if cause.isGone(current: current, onNow: Set(presetScene().on.map(\.key))) { clearPresetNotice() }
    }

    // MARK: - Save, Rename, Update, Delete

    /// Save as Preset…: with no speaker on, says so and saves nothing (S262).
    func promptSavePreset() {
        guard !presetScene().on.isEmpty else {
            menuBar?.close()
            showAlert(text.savePresetNeedsSpeakers, style: .informational)
            return
        }
        guard let typed = askPresetName(title: text.savePresetTitle, message: text.savePresetMessage,
                                        button: text.save),
              let saved = PresetEdit.saving(typed, scene: presetScene(), id: UUID().uuidString, to: presets,
                                            numbered: text.numberedPreset),
              let name = saved.last?.name else { return }
        presets = saved
        pushModel()
        AccessibilityNotification.Announcement(text.presetSaved(name)).post()
    }

    /// Rename…; a blank name keeps the current one.
    func promptRenamePreset(_ id: String) {
        guard let current = presets.first(where: { $0.id == id })?.name,
              let typed = askPresetName(title: text.renamePresetTitle, message: text.renameMessage(current),
                                        current: current, button: text.rename) else { return }
        let renamed = PresetEdit.renaming(id, to: typed, in: presets)
        guard renamed != presets else { return }
        presets = renamed
        // The notice names the preset, so it takes the new name in place.
        if let cause = presetNotice, cause.presetID == id, let preset = presets.first(where: { $0.id == id }),
           let message = PresetNotice.message(preset: preset.name, leftOff: cause.leftOff,
                                              nothingChanged: cause.nothingChanged, text: text) {
            postNotice(message, subject: Notice.presets, title: text.presetLeftOffTitle)
        }
        pushModel()
    }

    /// Update to Current Settings: what plays now, under the preset's ID,
    /// name and place. Asks nothing (S262).
    func updatePreset(_ id: String) {
        guard let name = presets.first(where: { $0.id == id })?.name else { return }
        guard let updated = PresetEdit.updating(id, scene: presetScene(), in: presets) else {
            menuBar?.close()
            showAlert(text.savePresetNeedsSpeakers, style: .informational)
            return
        }
        presets = updated
        pushModel()
        AccessibilityNotification.Announcement(text.presetUpdated(name)).post()
    }

    /// Delete: the later presets move up one shortcut and nothing that
    /// plays changes. Asks nothing (S262).
    func deletePreset(_ id: String) {
        guard let name = presets.first(where: { $0.id == id })?.name else { return }
        presets = PresetEdit.deleting(id, from: presets)
        if presetNotice?.presetID == id { clearPresetNotice() }
        pushModel()
        AccessibilityNotification.Announcement(text.presetDeleted(name)).post()
    }

    /// The name typed in a preset's name alert, which the user opened; nil
    /// when cancelled.
    private func askPresetName(title: String, message: String, current: String = "", button: String) -> String? {
        menuBar?.close()
        NSApp.activate(ignoringOtherApps: true)
        let prompt = NSAlert()
        prompt.messageText = title
        prompt.informativeText = message
        let field = NSTextField(frame: NSRect(x: 0, y: 0, width: 240, height: 24))
        field.stringValue = current
        field.placeholderString = text.presetPlaceholder
        field.setAccessibilityLabel(text.presetFieldLabel)
        prompt.accessoryView = field
        prompt.addButton(withTitle: button)
        prompt.addButton(withTitle: text.cancel)
        prompt.window.initialFirstResponder = field
        return prompt.runModal() == .alertFirstButtonReturn ? field.stringValue : nil
    }

    // MARK: - Window > Presets

    /// A Presets submenu item; each carries its preset's ID.
    @objc func recallPresetFromMenu(_ sender: Any?) {
        guard let id = (sender as? NSMenuItem)?.representedObject as? String else { return }
        recallPreset(id, from: .app)
    }

    @objc func savePreset(_ sender: Any?) { promptSavePreset() }
}
