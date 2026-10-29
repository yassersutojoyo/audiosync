import AppKit
import CAudioSyncCore
import ServiceManagement

// Zones, auto-connect and the login item.

extension AppDelegate {

    // MARK: - Zones

    /// A zone or preset member as the windows, a click on its zone and a
    /// recall see it.
    struct ZoneMemberState {
        let key: String
        let kind: SpeakerKey.Kind
        /// The row's ID: a device UID, a native AirPlay ID or "cast:<device ID>".
        let appID: String
        var facts: ZoneMemberFacts
        /// Listed now, hidden or not.
        var found = false
        /// The receiver ID of the stereo pair or home theater it plays as part of.
        var target: String? = nil
        var waiting = false
    }

    /// Hide Speaker keeps a Cast speaker under its app ID, every other one
    /// under its speaker key.
    func isHidden(_ key: String) -> Bool { hiddenSpeakers.contains(CastID.app(key: key) ?? key) }

    func memberState(_ key: String) -> ZoneMemberState? {
        guard let (id, kind) = SpeakerKey.parseAny(key) else { return nil }
        let hidden = isHidden(key)
        switch kind {
        case .local:
            let found = localDevices[id] != nil && localOrder.contains(id)
            return ZoneMemberState(key: key, kind: kind, appID: id,
                                   facts: ZoneMemberFacts(kind: .local, listed: !hidden && found,
                                                          on: enabledLocal.contains(id)),
                                   found: found)
        case .cast:
            let found = castModel.devices[id] != nil && castModel.rows.contains(id)
            return ZoneMemberState(key: key, kind: kind, appID: CastID.app(id),
                                   facts: ZoneMemberFacts(kind: .cast, listed: !hidden && found, on: castModel.enabled == id,
                                                          busy: castModel.cantStartNow(id)),
                                   found: found)
        case .airplay:
            let on = enabledAirPlay.contains(id)
            // Rows left from before the sender stopped aren't shown (`AppModel.shownAirPlay`).
            let found = !nativeStarts.stopped && airplayOrder.contains(id)
            let listed = !hidden && found
            let model = nativeAirPlay?.model
            let receiver = NativeAirPlayID.receiver(id)
            let isTarget = receiver.map { NativeTarget.kind($0) != nil } ?? false
            let overAirPlay2 = receiver.map { model?.reachedOverAirPlay2($0) == true } ?? false
            // The AirPlay 2 flag keeps the path on whatever the switch says (S246, S312).
            let airPlay2On = asc_airplay2_auto()
                || AppSettings.airPlay2On(setting: airPlay2AtLaunch, environment: ProcessInfo.processInfo.environment)
            // A member its target holds leaves the rows; the target's row stands for it.
            let target = listed ? nil : receiver.flatMap { model?.holdingTarget($0, on: on) }
            let facts = ZoneMemberFacts(kind: isTarget || overAirPlay2 ? .airplay2 : .airplay, listed: listed, on: on,
                                        needsAirPlay2: isTarget && !listed && !airPlay2On, inTarget: target != nil)
            return ZoneMemberState(key: key, kind: kind, appID: id, facts: facts, found: found, target: target,
                                   waiting: receiver.map { model?.isWaiting($0) == true } ?? false)
        }
    }

    /// What outside zone `keys` a click on it has to respect: a Cast speaker
    /// that is on, and an AirPlay 2 speaker that plays (S246, P169).
    private func zoneSituation(_ keys: Set<String>) -> (castOnElsewhere: Bool, airPlay2PlayingElsewhere: Bool) {
        let castElsewhere = castModel.enabled.map { !keys.contains(CastID.key($0)) } ?? false
        guard engine.isRunning, let model = nativeAirPlay?.model else { return (castElsewhere, false) }
        let airPlay2Elsewhere = enabledAirPlay.contains(where: { id in
            guard !keys.contains(SpeakerKey.make(id: id, kind: .airplay)),
                  let receiver = NativeAirPlayID.receiver(id) else { return false }
            return model.reachedOverAirPlay2(receiver) && !model.isWaiting(receiver)
        })
        return (castElsewhere, airPlay2Elsewhere)
    }

    private func clickPlan(_ members: [ZoneMemberState], keys: [String], streaming: Bool) -> ZoneClick.Action {
        let situation = zoneSituation(Set(keys))
        return ZoneClick.plan(members.map(\.facts), streaming: streaming, castOnElsewhere: situation.castOnElsewhere,
                              airPlay2PlayingElsewhere: situation.airPlay2PlayingElsewhere).action
    }

    /// A click on zone `id`, carried out as the core plans it (S246, S247):
    /// present members only, at most one Cast speaker, and every AirPlay 2
    /// member on, even one that waits for the AirPlay 2 place. The members in
    /// `keepOff` (a phone's click: they ask for a code) count as not listed
    /// and get a notice each when the zone turns on (S257).
    func toggleZone(_ id: String, keepingOff keepOff: [String] = [], origin: StartOrigin = .user) {
        calibrateZoneClicked()   // before the guard: a click that switches nothing ends a run too (S240)
        guard let zone = zones.first(where: { $0.id == id }) else { return }
        let members = zone.members.compactMap(memberState).map { member -> ZoneMemberState in
            var member = member
            if keepOff.contains(member.key) { member.facts.listed = false }
            return member
        }
        let situation = zoneSituation(Set(zone.members))
        let plan = ZoneClick.plan(members.map(\.facts), streaming: engine.isRunning,
                                  castOnElsewhere: situation.castOnElsewhere,
                                  airPlay2PlayingElsewhere: situation.airPlay2PlayingElsewhere)
        let turnOn: Bool
        switch plan.action {
        case .nothing: return
        case .turnOn, .start: turnOn = true
        case .turnOff: turnOn = false
        }
        DebugTrace.log("[AudioSync] the user switched zone \"\(zone.name)\" \(turnOn ? "on" : "off")")
        if turnOn { postPairFirst(keepOff.filter { zone.members.contains($0) }) }
        let running = engine.isRunning
        var attempts: [String] = []   // AirPlay members this click turns on
        for (member, step) in zip(members, plan.steps) {
            switch step {
            case .on, .waiting:
                // While streaming is off every member starts a new attempt;
                // one already on while streaming may be mid-pairing.
                if !running || !member.facts.on { switchZoneMember(member, on: true, attempts: &attempts) }
            case .off:
                if member.facts.on { switchZoneMember(member, on: false, attempts: &attempts) }
            case .castAnotherPlays, .castOneAtATime, .castBusy:
                DebugTrace.log("[AudioSync] zone \"\(zone.name)\": \(member.key) stays off (\(step))")
            case .absent, .needsAirPlay2, .inTarget:
                continue
            }
        }
        if let ids = origin.attemptsToBegin(attempts) { beginNativeAttempts(ids) }   // before the sender's speakers follow
        if origin == .remote { everyTimeGuard.turnedOnByPhone(attempts) }
        if turnOn && !engine.isRunning {
            startEngine(origin: origin)
        } else if engine.isRunning {
            applyAirPlaySelection()   // the sender's speakers follow the zone's switches
            syncLocalOutputs(origin: origin)
        }
        syncCast()
        stopIfIdle()
        retimeLocalOutputs()
        pushModel()
    }

    /// A notice for each speaker a phone's zone or preset left off because it
    /// asks for a code (S257): its prompt would wait on the Mac with nobody there.
    func postPairFirst(_ keys: [String]) {
        let listed = appModel.layout.thisMac + appModel.layout.connected
        for key in keys {
            guard let speaker = listed.first(where: { $0.key == key }) else { continue }
            DebugTrace.log("[AudioSync] web remote: \(key) stays off, it asks for a code")
            postNotice(text.remotePairFirst(speaker.name), subject: key)
        }
    }

    /// Switches one member as its own switch would, without starting or
    /// stopping streaming; switched on, it is a new attempt.
    func switchZoneMember(_ member: ZoneMemberState, on: Bool, attempts: inout [String]) {
        switch member.kind {
        case .local:
            if on {
                clearNotices([member.key])
                enabledLocal.insert(member.appID)
            } else {
                enabledLocal.remove(member.appID)
            }
        case .airplay:
            if on {
                clearNotices([member.key])
                enabledAirPlay.insert(member.appID)
                attempts.append(member.appID)
            } else {
                endNativeAttempt(member.appID)   // a pairing under way stops
                enabledAirPlay.remove(member.appID)
            }
        case .cast:
            guard let device = CastID.device(member.appID) else { return }
            if on {
                if castModel.turnOn(device) == nil { clearNotices([Notice.cast(device)]) }
            } else {
                castModel.turnOff(device)
            }
        }
    }

    /// The zones as the windows show them: hidden members neither listed
    /// nor counted, so the rows and the count agree.
    func zoneSnapshot() -> [AppModel.Zone] {
        zones.map { zone in
            let members = zone.members.filter { !isHidden($0) }.compactMap(memberState)
            let allOn = clickPlan(members, keys: zone.members, streaming: true) == .turnOff
            var shown = AppModel.Zone(id: zone.id, name: zone.name, volume: zone.volume, allOn: allOn)
            let anyOn = members.contains { $0.facts.listed && $0.facts.on }
            for member in members {
                if member.facts.listed {
                    shown.present += 1
                    if member.facts.on { shown.on += 1 }
                    if member.facts.on && member.waiting { shown.waiting += 1 }
                }
                shown.members.append(zoneMember(member, zoneOn: anyOn))
            }
            return shown
        }
    }

    private func zoneMember(_ member: ZoneMemberState, zoneOn: Bool) -> AppModel.ZoneMember {
        let name = memberName(member)
        var note: String?
        var target: AppModel.ZoneTarget?
        if member.facts.needsAirPlay2 {
            note = text.needsAirPlay2
        } else if let receiver = member.target {
            let targetID = NativeAirPlayID.app(receiver)
            let targetName = shownName(airPlay: targetID)
            note = text.zoneMemberInTarget(targetName)
            target = AppModel.ZoneTarget(key: SpeakerKey.make(id: targetID, kind: .airplay), name: targetName)
        } else if !member.facts.listed {
            note = text.zoneMemberNotFound
        } else if member.kind == .cast, !member.facts.on, zoneOn, castModel.enabled != nil {
            note = text.castStayedOff
        }
        return AppModel.ZoneMember(key: member.key, name: name, symbol: memberSymbol(member, name: name),
                                   speakerID: member.facts.listed ? member.appID : nil, note: note, target: target)
    }

    /// A member's name: the user's, else the one it is listed under, else
    /// the one its zone kept from an earlier listing (S286).
    func memberName(_ member: ZoneMemberState) -> String {
        let system = SpeakerZone.systemName(of: member.key, listed: listedName(member.key), in: zones)
        switch member.kind {
        case .airplay:
            return speakerNames.shown(key: SpeakerKey.make(id: member.appID, airplay: true),
                                      system: text.deviceName(system))
        case .cast:
            guard let device = CastID.device(member.appID) else { return text.deviceName(system) }
            return speakerNames.shown(key: CastID.key(device), system: text.deviceName(system))
        case .local: return shownName(local: member.appID, system: system)
        }
    }

    /// The name speaker `key` is listed under in this run, before the
    /// user's own; empty when there is none.
    private func listedName(_ key: String) -> String {
        guard let (id, kind) = SpeakerKey.parseAny(key) else { return "" }
        switch kind {
        case .airplay: return airplayNames[id] ?? ""
        case .cast:
            let announced = castModel.devices[id]?.name ?? ""
            return announced.isEmpty ? castModel.manualAddresses[id]?.text ?? "" : announced
        case .local: return localDevices[id]?.name ?? ""
        }
    }

    /// Keeps with its zone the name each member is listed under (S286), so a
    /// relaunch without it still shows that name.
    func rememberZoneNames() {
        var listed: [String: String] = [:]
        for key in zones.flatMap(\.members) {
            let name = listedName(key)
            if !name.isEmpty { listed[key] = name }
        }
        let named = SpeakerZone.remembering(listed, in: zones)
        if named != zones { zones = named }
    }

    private func memberSymbol(_ member: ZoneMemberState, name: String) -> String {
        switch member.kind {
        case .airplay: return SpeakerTag.airplay.symbol(name: name)
        case .cast: return SpeakerTag.cast.symbol(name: name)
        case .local:
            // The rows pick a local badge by the device's own name, as `Speaker.symbol` does.
            guard let device = localDevices[member.appID] else { return SpeakerTag.other.symbol(name: name) }
            return SpeakerTag(transport: device.transport).symbol(name: device.name.isEmpty ? name : device.name)
        }
    }

    /// New Zone from Active Speakers: the speakers that are on, in the
    /// windows' order, leave their zones for the new one.
    func promptNewZone() {
        let layout = appModel.layout
        let members = (layout.thisMac + layout.connected).filter(\.enabled).map(\.key)
        guard !members.isEmpty else {
            menuBar?.close()
            showAlert(text.newZoneNeedsSpeakers, style: .informational)
            return
        }
        guard let typed = askZoneName(title: text.newZoneTitle, message: text.newZoneMessage,
                                      button: text.create) else { return }
        addZone(named: typed, members: members)
    }

    /// New Zone… in a speaker's Move to Zone menu: a zone of that speaker.
    func promptNewZone(with key: String, name: String) {
        guard let typed = askZoneName(title: text.newZoneTitle, message: text.newZoneForSpeaker(name),
                                      button: text.create) else { return }
        addZone(named: typed, members: [key])
    }

    private func addZone(named typed: String, members: [String]) {
        let name = GroupName.forNewGroup(typed: typed, existing: zones.map(\.name), numbered: text.numberedZone)
        zones = ZoneEdit.adding(id: UUID().uuidString, name: name, members: members, to: zones)
        pushModel()
    }

    /// The name typed in zone `id`'s header; a blank name keeps the current one.
    func renameZone(_ id: String, to typed: String) {
        let renamed = ZoneEdit.renaming(id, to: typed, in: zones)
        guard renamed != zones else { return }
        zones = renamed   // saved by `zones`' didSet
        rereadCalibrateZoneSheet()
        pushModel()
    }

    /// The name typed in a zone's name alert, which the user opened; nil
    /// when cancelled.
    private func askZoneName(title: String, message: String, button: String) -> String? {
        menuBar?.close()
        NSApp.activate(ignoringOtherApps: true)
        let prompt = NSAlert()
        prompt.messageText = title
        prompt.informativeText = message
        let field = NSTextField(frame: NSRect(x: 0, y: 0, width: 240, height: 24))
        field.placeholderString = text.newZonePlaceholder
        field.setAccessibilityLabel(text.newZoneFieldLabel)
        prompt.accessoryView = field
        prompt.addButton(withTitle: button)
        prompt.addButton(withTitle: text.cancel)
        prompt.window.initialFirstResponder = field
        return prompt.runModal() == .alertFirstButtonReturn ? field.stringValue : nil
    }

    /// Delete Zone: its members return to the plain lists; nothing else changes.
    func deleteZone(_ id: String) {
        guard zones.contains(where: { $0.id == id }) else { return }
        zones.removeAll { $0.id == id }
        appModel.calibrateAll = CalibrateAllFlow.deletingZone(id, from: appModel.calibrateAll)
        pushModel()
    }

    func moveToZone(_ key: String, _ id: String) {
        zones = ZoneEdit.moving(key, to: id, in: zones)
        pushModel()
    }

    func removeFromZone(_ key: String) {
        zones = ZoneEdit.removing(key, from: zones)
        pushModel()
    }

    /// Only This Speaker in a member's ⋯ menu under its zone: each switch
    /// as the member's own would make it, streaming started or stopped once.
    func onlyThisSpeaker(_ key: String) {
        guard let zone = zones.first(where: { $0.members.contains(key) }) else { return }
        let members = zone.members.compactMap(memberState)
        let steps = ZoneLayout.onlyThis(key, members: members.map {
            ZoneLayout.Member(key: $0.key, listed: $0.facts.listed, on: $0.facts.on, cast: $0.kind == .cast,
                              busy: $0.facts.busy)
        })
        guard !steps.isEmpty else {
            DebugTrace.log("[AudioSync] only \(key): nothing switches")
            return
        }
        DebugTrace.log("[AudioSync] only \(key) plays in zone \"\(zone.name)\"")
        var attempts: [String] = []
        for step in steps {
            guard let member = members.first(where: { $0.key == step.key }) else { continue }
            switchZoneMember(member, on: step.on, attempts: &attempts)
        }
        beginNativeAttempts(attempts)
        if steps.contains(where: \.on) && !engine.isRunning {
            startEngine(origin: .user)
        } else if engine.isRunning {
            applyAirPlaySelection()
            syncLocalOutputs(origin: .user)
        }
        syncCast()
        stopIfIdle()
        retimeLocalOutputs()
        pushModel()
    }

    /// Use <target> Instead (S247): the target takes the member's place,
    /// and the target's other members in the zone leave it.
    func useTargetInstead(in id: String, for key: String) {
        guard let receiver = memberState(key)?.target else { return }
        let target = SpeakerKey.make(id: NativeAirPlayID.app(receiver), kind: .airplay)
        let members = (nativeAirPlay?.model.members(of: receiver) ?? [])
            .map { SpeakerKey.make(id: NativeAirPlayID.app($0), kind: .airplay) }
        zones = ZoneEdit.usingTarget(target, for: Set(members + [key]), in: id, zones: zones)
        pushModel()
    }

    // MARK: - Auto-connect, login item

    /// Speakers marked auto-connect that just appeared (or were there at launch):
    /// switch them on, then start streaming once for all of them. Speakers still
    /// on from the last session only need the stream started.
    func autoConnect(_ keys: [String]) {
        guard !LaunchAutoConnect.skipped else { return }
        let hits = hiddenSpeakers.autoConnecting(keys, marked: autoConnect)
        guard !hits.isEmpty else { return }
        DebugTrace.log("[AudioSync] auto-connect \(hits)")
        for key in hits where !isEnabled(key) {
            guard let (id, airplay) = SpeakerKey.parse(key) else { continue }
            // Switched on automatically: an AirPlay speaker's code request becomes a notice.
            if airplay { enabledAirPlay.insert(id) } else { enabledLocal.insert(id) }
        }
        if !engine.isRunning, !userStopped, !autoStartFailed {
            startEngine(origin: .automatic)
        } else if engine.isRunning {
            applyAirPlaySelection()   // the speakers just switched on join the session under way
            syncLocalOutputs(origin: .automatic)
        }
        pushModel()
    }

    private func isEnabled(_ key: String) -> Bool {
        guard let (id, airplay) = SpeakerKey.parse(key) else { return false }
        return airplay ? enabledAirPlay.contains(id) : enabledLocal.contains(id)
    }

    func setLaunchAtLogin(_ on: Bool) {
        let service = SMAppService.mainApp
        clearNotices([Notice.login])   // a new attempt
        do {
            if on { try service.register() } else { try service.unregister() }
        } catch {
            DebugTrace.log("[AudioSync] login item: \(error)")
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
