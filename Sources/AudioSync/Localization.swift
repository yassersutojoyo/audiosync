import Foundation

/// Every user-facing string in one place, so a translation can be added later
/// without hunting through view code.
///
/// Language decision: English only for now, as plain Swift strings. When a
/// second language is added, move these to a String Catalog
/// (`Localizable.xcstrings`) behind the same properties and functions, so the
/// call sites stay unchanged. The `NS…UsageDescription` strings in Info.plist
/// are localized by the system through `InfoPlist.strings`, not here.
///
/// Engine code never builds user-facing text: it reports typed reasons
/// (`SourceCapture.CaptureError`, `AirPlayPairing.Reason`) that are mapped
/// to messages here. Raw Swift errors and OSStatus codes go to the debug log only.
struct UIStrings {

    /// How numbers such as percentages are written: the user's locale (the
    /// logic suite fixes one).
    var locale = Locale.autoupdatingCurrent

    // MARK: Main window

    let appName = "AudioSync"
    let mainWindowTitle = "AudioSync"
    let streamingSwitch = "Streaming"
    let turnOn = "Turn On"
    let turnOff = "Turn Off"
    let on = "On"
    let off = "Off"
    /// A speaker's accessibility value while it waits for streaming to start.
    let willPlay = "Will play"
    /// Tooltip on a speaker that waits for streaming to start.
    let willPlayHelp = "Plays when streaming starts. Click to start streaming."
    let statusOff = "Off"
    let retry = "Retry"
    let retryAirPlayHelp = "Start AudioSync's own AirPlay sender again"
    /// AudioSync's own AirPlay sender stopped after an internal error; shown
    /// in place of the AirPlay speakers, with Retry.
    let nativeAirPlayFailed = "AudioSync's own AirPlay sender stopped after an internal error."
    let alertOK = "OK"
    let guideTitle = "AudioSync Guide"
    let refreshDevices = "Refresh Devices"
    let showVirtualDevices = "Show Virtual Devices"
    let appearance = "Appearance"
    func appearanceName(_ choice: AppAppearance) -> String {
        switch choice {
        case .system: return "Match System"
        case .light: return "Light"
        case .dark: return "Dark"
        }
    }
    let masterVolumeTitle = "Master Volume"
    let moreOptions = "Options"
    let dismissNotice = "Dismiss Notice"
    let dismissAllNotices = "Dismiss All"
    let previousTrack = "Previous Track"
    let nextTrack = "Next Track"
    let play = "Play"
    let pause = "Pause"
    /// What AirPlay speakers with a screen, such as a TV, show while AudioSync
    /// doesn't know what is playing: AudioSync's name as the title, the source's name as the source
    /// picker shows it as the artist (`receiverUnnamedSource` when there is no
    /// name to show) and `receiverAlbum` as the album. None of them may be
    /// blank: a TV may then show no text at all.
    var receiverUntitled: String { appName }
    let receiverUnnamedSource = "Live audio"
    let receiverAlbum = "From your Mac"
    let delayFieldLabel = "Delay in milliseconds"
    /// Under a delay field while it holds typing not yet applied; also the
    /// field's VoiceOver hint then.
    let delayDraftHint = "Press Return to apply"
    /// Under a delay field, in place of `delayDraftHint`, while it holds a
    /// number out of the delay's range; also the field's VoiceOver hint then.
    func delayOutOfRange(lowest: String, highest: String, applies: String) -> String {
        "The range is \(lowest) to \(highest) \(millisecondsUnit). Return applies \(applies)."
    }
    /// Under a delay field, for a moment, after it blocked a keystroke it
    /// can't take; VoiceOver announces it.
    let delayFieldTakes = "Only digits, with + or − in front"

    func statusStreaming(speakers: Int) -> String {
        switch speakers {
        case 0: return "Streaming"
        case 1: return "Streaming to 1 speaker"
        default: return "Streaming to \(speakers) speakers"
        }
    }

    /// The link under the newest notice in the menu bar panel to the window
    /// that lists the others.
    func moreNotices(_ count: Int) -> String {
        count == 1 ? "1 more notice…" : "\(count) more notices…"
    }

    /// The menu bar icon's accessibility value while notices wait.
    func noticesWaiting(_ count: Int) -> String {
        count == 1 ? "1 notice" : "\(count) notices"
    }

    func speakerOptions(name: String) -> String { "Options for \(name)" }

    /// A speaker's (or group's) accessibility value.
    func value(_ state: SpeakerState) -> String {
        switch state {
        case .off: return off
        case .willPlay: return willPlay
        case .playing: return on
        }
    }

    /// What clicking a speaker (or group) does, as its tooltip.
    func help(_ state: SpeakerState) -> String {
        switch state {
        case .off: return turnOn
        case .willPlay: return willPlayHelp
        case .playing: return turnOff
        }
    }

    /// A speaker's (or group's) VoiceOver hint: its note, if any, then what a
    /// click does while it waits for streaming to start.
    func rowHint(_ state: SpeakerState, note: String?) -> String {
        [note, state == .willPlay ? willPlayHelp : nil]
            .compactMap { $0 }
            .joined(separator: ". ")
    }

    /// The note next to an AirPlay speaker that can't simply play; nil when it can.
    func availability(_ a: SpeakerAvailability) -> String? {
        switch a {
        case .open: return nil
        case .needsCode: return "Asks for a code"
        }
    }

    /// Tooltip for that note.
    func availabilityHelp(_ a: SpeakerAvailability) -> String? {
        switch a {
        case .open: return nil
        case .needsCode:
            return "This speaker asks for a PIN or password. AudioSync asks you for it when you turn the speaker on."
        }
    }
    func volume(of name: String) -> String { "\(name) volume" }
    /// A 0–100 volume as shown next to its slider and read by VoiceOver.
    func volumeValue(_ volume: Double) -> String { "\(Int(volume.rounded()))" }
    /// A delay in whole milliseconds, the unit the sync field and steps use.
    func delayValue(ms: Double) -> String { "\(Int(ms.rounded())) \(millisecondsUnit)" }
    /// An offset relative to the reference in whole ms, with its sign, as its
    /// field shows it: "+150", "−250", "0" (S181).
    func offsetNumber(_ ms: Double) -> String {
        let n = Int(ms.rounded())
        return n > 0 ? "+\(n)" : n < 0 ? "−\(-n)" : "0"
    }
    /// An offset as its speaker's row shows it and VoiceOver reads it: "+150 ms".
    func offsetValue(ms: Double) -> String { "\(offsetNumber(ms)) \(millisecondsUnit)" }
    func delay(of name: String) -> String { "\(name) delay" }
    /// VoiceOver, as a delay applies while its panel is open: "Desk delay set to +150 ms".
    func delaySet(_ label: String, to value: String) -> String { "\(label) set to \(value)" }
    func delayStep(_ delta: Int) -> String { delta < 0 ? "−\(-delta)" : "+\(delta)" }
    func delayStepAccessibility(_ delta: Int) -> String {
        delta < 0 ? "Decrease delay by \(-delta) \(millisecondsUnit)"
                  : "Increase delay by \(delta) \(millisecondsUnit)"
    }

    // MARK: Microphone calibration (S232)

    let calibrateWithMicrophone = "Calibrate with Microphone"
    func calibrateLabel(of name: String) -> String { "Calibrate \(name) with the microphone" }
    let calibrationHint = "Put the microphone where you listen. AudioSync pauses the music, plays a short, "
        + "quiet sweep on the built-in speakers and then on this speaker, and sets this speaker's delay from the difference."
    let calibrationCancel = "Cancel"
    func calibrationCancelLabel(of name: String) -> String { "Cancel calibrating \(name)" }
    let openMicrophoneSettings = "Open Microphone Settings"

    /// What a running calibration is doing.
    func calibrationStep(_ step: CalibrationPhase.Step) -> String {
        switch step {
        case .preparing: "Pausing the music…"
        case .measuring(.reference): "Listening to the built-in speakers…"
        case .measuring(.chosen): "Listening to this speaker…"
        case .estimating: "Working out the delay…"
        }
    }

    /// The line under the button once a calibration ended; nil while none
    /// did or one runs.
    func calibrationMessage(_ phase: CalibrationPhase) -> String? {
        switch phase {
        case .idle, .running:
            return nil
        case .done(let ms, let clamped):
            return clamped
                ? "Delay set to \(offsetValue(ms: ms)), the end of the range. The measured difference is larger, "
                    + "so this speaker stays a little out of sync."
                : "Delay set to \(offsetValue(ms: ms))."
        case .failed(let failure):
            return calibrationFailure(failure) + " The delay wasn't changed."
        }
    }

    func calibrationFailure(_ failure: CalibrationFailure) -> String {
        switch failure {
        case .noReference:
            return "Calibration measures against the Mac's built-in speakers, and they aren't listed."
        case .notPlaying:
            return "Turn on streaming, this speaker and the built-in speakers first."
        case .microphoneDenied:
            return "AudioSync isn't allowed to use the microphone. Turn it on in System Settings, "
                + "Privacy & Security, Microphone."
        case .noMicrophone:
            return "AudioSync couldn't record from the microphone. Check the input in System Settings, Sound."
        case .unsupportedRate:
            return "The audio source runs at a sample rate calibration can't use."
        case .sourceSilent:
            return "The audio source isn't sending audio, so the test sound couldn't play. "
                + "Play something, or choose \(sourceSystem) as the source, and try again."
        case .interrupted:
            return "Streaming, the speakers or their delays changed during the measurement."
        case .tooShort:
            return "The recording was too short. Try again."
        case .tooLong:
            return "The delays are too long to measure in one recording. Lower the largest delay and try again."
        case .clipped(let which):
            return "The microphone was overloaded while \(calibrationSubject(which)) played. "
                + "Turn the volume down or move the microphone farther away, and try again."
        case .tooQuiet(let which):
            return "The microphone barely heard \(calibrationSubject(which)). "
                + "Turn the volume up or move the microphone closer, and try again."
        case .ambiguousEcho(let which):
            return "An echo or another sound was almost as loud as \(calibrationSubject(which)). "
                + "Give the microphone a clear line to both speakers and try again."
        case .implausible:
            return "The two speakers were too far apart to measure."
        }
    }

    private func calibrationSubject(_ which: CalibrationRecording?) -> String {
        switch which {
        case .reference: "the built-in speakers"
        case .chosen: "this speaker"
        case nil: "a speaker"
        }
    }

    // MARK: Main menu

    let menuAbout = "About AudioSync"
    let aboutCopyright = "Copyright © 2026 Yasser Sutojoyo Kusumo"
    let aboutLicense = "Licensed under GPL-3.0-or-later. Comes with no warranty."
    let menuServices = "Services"
    let menuHide = "Hide AudioSync"
    let menuHideOthers = "Hide Others"
    let menuShowAll = "Show All"
    let menuEdit = "Edit"
    let menuUndo = "Undo"
    let menuRedo = "Redo"
    let menuCut = "Cut"
    let menuCopy = "Copy"
    let menuPaste = "Paste"
    let menuSelectAll = "Select All"
    let menuSpeakers = "Speakers"
    let menuTurnStreamingOn = "Turn Streaming On"
    let menuTurnStreamingOff = "Turn Streaming Off"
    let menuFile = "File"
    let menuCloseWindow = "Close Window"
    /// Command-S: applies the delay typed in an open delay panel.
    let menuApplyDelay = "Apply Typed Delay"
    let menuWindow = "Window"
    let menuMinimize = "Minimize"
    let menuZoom = "Zoom"
    let menuBringAllToFront = "Bring All to Front"
    let menuHelp = "Help"

    // MARK: Sources

    let sourceLabel = "Audio Source"
    let sourceSystem = "All System Audio"
    let sourceInputsHeader = "Input Devices"
    let microphoneDenied = """
        AudioSync is not allowed to use input devices.
        Open System Settings → Privacy & Security → Microphone and turn on AudioSync.
        """

    func sourceTerminated(name: String) -> String {
        "\(name) quit, so streaming has stopped."
    }

    /// The source app quit and the source became the whole system (S227);
    /// `started` false when streaming couldn't start with it.
    func sourceSwitchedToSystem(name: String, started: Bool) -> String {
        started ? "\(name) quit, so AudioSync switched to \(sourceSystem)."
            : "\(name) quit, so the source is now \(sourceSystem), but streaming couldn't start with it."
    }
    var switchToSystemWhenSourceQuits: String { "Switch to \(sourceSystem) When the Source App Quits" }

    /// An AirPlay speaker was switched off by AudioSync, not by the user.
    func airPlayTurnedOff(name: String, reason: AirPlayPairing.Reason) -> String {
        switch reason {
        case .unreachable:
            return "\(name) couldn't be reached, so it was turned off. "
                + "Check that the speaker is on and on the same network, then turn it on again."
        case .needsCode:
            return "\(name) asks for a PIN or password, so it was turned off. "
                + "To pair it, turn it on again and enter the code when AudioSync asks. "
                + "To use it without a code, set Allow Speaker & TV Access on the speaker (or in the Home app) "
                + "to \"Anyone on the Same Network\" and turn off Require Password."
        case .cancelled:
            return "Pairing with \(name) was cancelled, so it was turned off. "
                + "To try again, turn it on and enter its PIN or password when AudioSync asks."
        case .stillNeedsCode:
            return "\(name) still asks for a PIN or password after pairing, so it was turned off. "
                + "On the speaker (or in the Home app), check that Allow Speaker & TV Access is set to "
                + "\"Anyone on the Same Network\", then turn \(name) on again in AudioSync."
        }
    }

    /// A PIN pairing ended without pairing, for another reason than a wrong
    /// PIN (which asks again).
    func nativeAirPlayPairingFailed(name: String) -> String {
        "Pairing with \(name) didn't finish, so it was turned off. "
            + "To try again, turn it on and enter the PIN it shows."
    }

    /// AudioSync's own AirPlay sender couldn't start (or browse the network
    /// for speakers). AudioSync tries again by itself.
    let nativeAirPlayCouldNotStart = "AudioSync's own AirPlay sender couldn't start, so no AirPlay speakers "
        + "are listed. AudioSync keeps trying. If this doesn't go away, restart AudioSync."

    /// The notice when AudioSync's own AirPlay sender stopped after an
    /// internal error.
    let nativeAirPlayStopped = "AudioSync's own AirPlay sender stopped after an internal error, "
        + "so AirPlay speakers aren't playing. Click Retry under the speakers to start it again. "
        + "If it keeps stopping, restart AudioSync."

    /// AudioSync's own AirPlay sender can't play to this receiver.
    func nativeAirPlayCantPlay(name: String) -> String {
        "\(name) can't play from AudioSync's own AirPlay sender yet, so it was turned off."
    }

    // MARK: AirPlay pairing prompt

    func pairTitle(name: String) -> String { "Pair with \(name)" }

    func pairPasswordMessage(name: String) -> String {
        "\(name) is protected by a password. Enter the password set for it "
            + "in the Home app or in the speaker's settings. "
            + "Cancel turns the speaker off; turn it on again to retry."
    }

    /// The password prompt again, after the receiver turned the last one down.
    func pairWrongPasswordMessage(name: String) -> String {
        "\(name) didn't accept that password. Enter the password set for it "
            + "in the Home app or in the speaker's settings. "
            + "Cancel turns the speaker off; turn it on again to retry."
    }

    /// AudioSync's own AirPlay sender pairs an Apple TV with the PIN it shows.
    func nativePairPINMessage(name: String) -> String {
        "\(name) shows a PIN on the TV. Enter it to pair; after that, it connects without a PIN. "
            + "Cancel turns the speaker off; turn it on again to retry."
    }

    /// The native PIN prompt again, after a PIN that didn't work (an Apple
    /// TV then shows a new one).
    func nativePairWrongPINMessage(name: String) -> String {
        "That PIN didn't work. Enter the PIN \(name) shows on the TV now. "
            + "Cancel turns the speaker off; turn it on again to retry."
    }

    let pairPINOnlyPlaceholder = "PIN"
    let pairPasswordPlaceholder = "Password"
    func pairPINFieldLabel(name: String) -> String { "PIN for \(name)" }
    func pairPasswordFieldLabel(name: String) -> String { "Password for \(name)" }
    let pair = "Pair"

    /// Capture stopped on its own while streaming.
    func sourceStopped(_ error: SourceCapture.CaptureError) -> String {
        switch error {
        case .inputDisconnected, .inputNotFound:
            return "The input device was disconnected, so streaming has stopped. "
                + "Plug it in again, or pick another source."
        case .appNotRunning(let bundleID):
            return sourceTerminated(name: SourceApps.name(for: bundleID))
        case .ownProcessUnknown, .tapNotCreated, .formatUnusable, .aggregateDeviceFailed, .ioProcNotStarted:
            return "AudioSync lost the audio from the selected source, so streaming has stopped. "
                + "Turn streaming on again, or pick another source."
        }
    }

    // MARK: Alerts

    let chooseSpeakerBeforeStart = "Turn on at least one speaker first (under This Mac or Connected Speakers)."

    /// Streaming is on but the source hasn't played anything yet.
    func waitingForAudio(_ app: String?) -> String {
        if let app { return "Waiting for audio from \(app). Start playback there and it streams right away." }
        return "Waiting for audio. Play something and it streams right away."
    }
    let captureNotWorking = """
        Audio is not being captured.
        Open System Settings → Privacy & Security → Screen & System Audio Recording and turn \
        on AudioSync under "System Audio Recording Only", then play something from the \
        selected source. If you just allowed access, turn streaming off and on again.
        """

    let unknownDevice = "Unknown device"
    /// A device's name, or a placeholder when Core Audio can't read it.
    func deviceName(_ name: String) -> String { name.isEmpty ? unknownDevice : name }

    func outputCouldNotStart(names: String) -> String {
        let deviceName = names.isEmpty ? unknownDevice : names
        return "Could not start output for this speaker, so it was turned off: \(deviceName)\n"
            + "Another app may be using it exclusively, or the format may be incompatible."
    }

    func streamingStartMessage(for error: Error) -> String {
        if let reason = error as? SourceCapture.CaptureError {
            return captureFailureText(reason)
        }
        if let reason = error as? StreamEngine.StartError {
            switch reason {
            case .coreUnavailable: return engineSetupFailed
            }
        }
        return captureFailed
    }

    /// The audio core (linked into the app) couldn't build a session's parts,
    /// most likely because memory ran out.
    let engineSetupFailed = "AudioSync couldn't set up its audio engine, so streaming can't start. "
        + "Quit AudioSync and open it again. If it keeps happening, restart your Mac."

    private let captureFailed = "AudioSync couldn't capture audio from the selected source. "
        + "Turn streaming off and on again, or pick another source. If it keeps happening, restart your Mac."

    /// Status codes stay out of these messages; the caller logs them.
    private func captureFailureText(_ error: SourceCapture.CaptureError) -> String {
        switch error {
        case .appNotRunning(let bundleID):
            return "\(SourceApps.name(for: bundleID)) is not running. Open it first, or pick another source."
        case .ownProcessUnknown:
            return "Could not find AudioSync's own audio process. Please relaunch the app."
        case .tapNotCreated, .formatUnusable, .aggregateDeviceFailed, .ioProcNotStarted:
            return captureFailed
        case .inputNotFound, .inputDisconnected:
            return "The selected input device is not connected. Plug it in, or pick another source."
        }
    }

    // MARK: Speakers, groups, menu bar

    let thisMacSection = "This Mac"
    let connectedSection = "Connected Speakers"
    let airPlayNoneFound = "No AirPlay speakers found on this network"
    let thisMacNone = "No built-in output"

    /// A connected speaker's kind of output: the type icon's tooltip and part
    /// of the row's VoiceOver label.
    func speakerTag(_ tag: SpeakerTag) -> String {
        switch tag {
        case .airplay: return "AirPlay"
        case .systemAirPlay: return "macOS AirPlay"
        case .builtIn: return "Built-in"
        case .bluetooth: return "Bluetooth"
        case .usb: return "USB"
        case .hdmi: return "HDMI"
        case .displayPort: return "DisplayPort"
        case .thunderbolt: return "Thunderbolt"
        case .firewire: return "FireWire"
        case .pci: return "PCI"
        case .avb: return "AVB"
        case .virtual: return "Virtual"
        case .aggregate: return "Aggregate"
        case .other: return "Other"
        }
    }

    /// A speaker row's VoiceOver label: "Kitchen, Bluetooth".
    func speakerLabel(name: String, tag: String?) -> String {
        guard let tag else { return name }
        return "\(name), \(tag)"
    }
    /// The same, followed for a renamed speaker by the name it reports.
    func speakerLabel(name: String, tag: String?, renamedFrom system: String?) -> String {
        let label = speakerLabel(name: name, tag: tag)
        guard let system else { return label }
        return "\(label), \(renamedSpeaker(system: system))"
    }

    // The output device volume hint (DeviceVolume.swift, S198 to S200),
    // worded like the Windows app's with macOS in place of Windows.

    /// A whole percent as the user's locale writes it: "13%".
    func percent(_ value: Int) -> String {
        (Double(value) / 100).formatted(.percent.precision(.fractionLength(0)).locale(locale))
    }

    /// The hint under a switched-on local output's volume slider, and the
    /// tooltip of the compact panel's glyph.
    func deviceVolumeHint(_ hint: DeviceVolumeHint) -> String {
        switch hint {
        case .low(let n): return "macOS volume is \(percent(n))"
        case .muted: return "Muted in macOS"
        case .mutedAndLow(let n): return "Muted in macOS, volume \(percent(n))"
        }
    }

    /// The one-click fix after the hint.
    func deviceVolumeFix(_ hint: DeviceVolumeHint) -> String {
        switch hint {
        case .low: return "Set to \(percent(DeviceVolumeHint.fixPercent))"
        case .muted: return "Unmute"
        case .mutedAndLow: return "Unmute at \(percent(DeviceVolumeHint.fixPercent))"
        }
    }

    /// The hint's VoiceOver label, naming the output:
    /// "MacBook Pro Speakers: macOS volume is 13%".
    func deviceVolumeHintLabel(_ hint: DeviceVolumeHint, name: String) -> String {
        switch hint {
        case .low(let n): return "\(name): macOS volume is \(percent(n))"
        case .muted: return "\(name): muted in macOS"
        case .mutedAndLow(let n): return "\(name): muted in macOS, volume \(percent(n))"
        }
    }

    /// The fix's VoiceOver label: its visible text first, so Voice Control
    /// finds it by what is on screen, then the output.
    func deviceVolumeFixLabel(_ hint: DeviceVolumeHint, name: String) -> String {
        "\(deviceVolumeFix(hint)), \(name)"
    }

    /// The tooltip of the hint in the main window.
    let deviceVolumeHelp = "macOS has its own volume for each output, and it limits AudioSync's slider."

    let noSpeakers = "No speakers found"
    let masterVolume = "Master volume"
    let openApp = "Open AudioSync"
    let quitApp = "Quit AudioSync"
    /// The menu bar panel's short Quit, next to Open AudioSync.
    let quitShort = "Quit"
    let equalizer = "Equalizer"
    let groups = "Groups"
    let newGroup = "New Group from Active Speakers…"
    let newGroupTitle = "New Speaker Group"
    let newGroupMessage = "The speakers that are on right now become this group. Click the group later to switch them all at once."
    let newGroupPlaceholder = "Living room, Office…"
    /// VoiceOver label of the name field in the New Speaker Group alert.
    let newGroupFieldLabel = "Group name"
    let newGroupNeedsSpeakers = "Turn on the speakers you want in the group first, then create it."
    let deleteGroup = "Delete Group"
    let renameGroup = "Rename Group…"
    let renameGroupTitle = "Rename Group"
    func renameGroupMessage(_ name: String) -> String { "Enter a new name for “\(name)”." }
    let rename = "Rename"
    /// In a speaker's ⋯ menu and right-click menu: edits its name in the row.
    let renameSpeaker = "Rename…"
    /// The name field's VoiceOver label; the field's placeholder is the
    /// system name, which an empty field brings back.
    func speakerNameField(system: String) -> String { "Name for \(system)" }
    /// The tooltip on a speaker's name.
    func speakerNameHelp(system: String, renamed: Bool) -> String {
        renamed ? "Device name: \(system). Double-click to rename." : "Double-click to rename."
    }
    /// What VoiceOver adds after a renamed speaker's label.
    func renamedSpeaker(system: String) -> String { "device name \(system)" }
    /// A new group's name when none was typed.
    func numberedGroup(_ n: Int) -> String { "Group \(n)" }
    let create = "Create"
    let cancel = "Cancel"
    let autoConnect = "Auto-connect when available"
    let launchAtLogin = "Launch at Login"
    let launchAtLoginFailed = "macOS did not allow AudioSync to launch at login. You can add it in System Settings → General → Login Items & Extensions."
    func automationDenied(_ app: String) -> String {
        "AudioSync isn't allowed to control \(app), so Now Playing can't show its track or controls, and AirPlay speakers with a screen can't show it either. Turn on \(app) under AudioSync in System Settings → Privacy & Security → Automation."
    }
    let openSettings = "Open Settings"
    let openSettingsHint = "Opens System Settings at the right privacy setting"
    /// AirPlay has found nothing for a while and AudioSync can't tell whether
    /// Local Network access is the reason.
    let localNetworkHint = "If you expect AirPlay speakers, check that AudioSync is allowed in System Settings → Privacy & Security → Local Network."
    /// The menu bar panel's one-line version of `localNetworkHint`.
    let localNetworkHintShort = "Speakers missing? Check Local Network access."
    /// macOS denies AudioSync Local Network access.
    let localNetworkDenied = "AudioSync isn't allowed on the local network, so it can't find AirPlay speakers. Turn on AudioSync in System Settings → Privacy & Security → Local Network."
    /// The menu bar panel's one-line version of `localNetworkDenied`.
    let localNetworkDeniedShort = "Local Network access is off for AudioSync."
    /// The hint's text in the window (`short`: in the menu bar panel); nil: none.
    func localNetworkHint(_ hint: LocalNetworkHint, short: Bool) -> String? {
        switch hint {
        case .none: return nil
        case .denied: return short ? localNetworkDeniedShort : localNetworkDenied
        case .possible: return short ? localNetworkHintShort : localNetworkHint
        }
    }
    let openLocalNetworkSettings = "Open Local Network Settings"
    let launchAtLoginNeedsApproval = "Allow AudioSync in System Settings → General → Login Items & Extensions to finish turning on Launch at Login."

    func groupMembers(_ available: Int, of total: Int) -> String {
        available == total ? "\(total) speaker\(total == 1 ? "" : "s")" : "\(available) of \(total) speakers available"
    }
    let syncAdjust = "Adjust sync"
    /// The hint under a local speaker's delay field: what the value is
    /// relative to (S181). Without the Mac's built-in speakers listed, the
    /// values only count against each other.
    func offsetHint(hasReference: Bool) -> String {
        (hasReference
            ? "Relative to the built-in speakers. Use a negative value for a speaker that sounds late, such as Bluetooth."
            : "The speaker with the lowest value plays first; the others wait by the difference.")
            + " Heard this speaker first? Raise the value. Heard it late? Lower it."
    }
    /// The AirPlay delay (S178, S181): one field for every local speaker.
    let airPlayDelay = "AirPlay delay"
    let airPlayDelayHint = "While an AirPlay speaker is on, local speakers wait this long for it. "
        + "Heard the AirPlay speakers late? Raise it."
    let millisecondsUnit = "ms"

    // MARK: Equalizer

    let equalizerTitle = "Equalizer"
    let equalizerCustom = "Custom"
    let equalizerReset = "Reset"
    let equalizerPreamp = "Pre"
    func equalizerCaption(maxGain: Int) -> String { "±\(maxGain) dB · applied to every speaker" }
    let equalizerPreampAccessibility = "Preamp"

    func equalizerPreset(_ id: Equalizer.PresetID) -> String {
        switch id {
        case .flat: return "Flat"
        case .bassBoost: return "Bass Boost"
        case .bassCut: return "Bass Cut"
        case .trebleBoost: return "Treble Boost"
        case .vocal: return "Vocal"
        case .rock: return "Rock"
        case .pop: return "Pop"
        case .jazz: return "Jazz"
        case .classical: return "Classical"
        case .electronic: return "Electronic"
        case .loudness: return "Loudness"
        case .smallSpeakers: return "Small Speakers"
        }
    }

    /// A band's short label under its slider: "32", "125", "1k", "16k".
    func equalizerBandShort(hz: Double) -> String {
        hz >= 1000 ? "\(Int(hz / 1000))k" : "\(Int(hz))"
    }

    /// A band's spoken name: "125 Hz", "1 kHz".
    func equalizerBandName(hz: Double) -> String {
        hz >= 1000 ? "\(Int(hz / 1000)) kHz" : "\(Int(hz)) Hz"
    }

    /// A gain shown above its slider: "+2.5", "+3", "0", "−1.5". One decimal
    /// only when the value isn't whole (the sliders move in 0.5 dB steps).
    func equalizerGain(_ db: Double) -> String {
        let tenths = (db * 10).rounded()
        if tenths == 0 { return "0" }
        let magnitude = abs(tenths) / 10
        let digits = magnitude == magnitude.rounded()
            ? String(Int(magnitude))
            : String(format: "%.1f", magnitude)
        return (tenths > 0 ? "+" : "−") + digits
    }

    /// A gain as VoiceOver reads it: "+2.5 dB".
    func equalizerGainValue(_ db: Double) -> String { "\(equalizerGain(db)) dB" }

    // MARK: Guide

    var helpText: String { """
        AudioSync Guide
        By Yasser Sutojoyo Kusumo

        AudioSync plays your Mac's sound (the whole system, one app, or an input device)
        through HomePod/AirPlay speakers and local speakers connected to this Mac
        (jack, HDMI, USB-C, Bluetooth), then aligns them with a per-speaker delay
        relative to the Mac's built-in speakers.

        First Steps
        1. Open AudioSync. Its window holds every control. The AudioSync icon in
           the menu bar gives quick access to the power switch, the source, the
           volume and the speakers. Closing the window keeps AudioSync running in
           the menu bar, and streaming continues.
        2. Pick a source under 'Audio Source': All System Audio, a specific app,
           or an input device.
           Keep the macOS sound output on a local device. Don't pick a HomePod in
           Control Center or in the player app: AudioSync streams to AirPlay itself.
        3. For AirPlay, check HomePod access in the Home app:
           Home Settings → Speakers & TV → Allow Speaker & TV Access →
           "Anyone on the Same Network".
        4. Speakers are listed under 'This Mac' (the Mac's own speakers and
           headphone jack), then your groups, then 'Connected Speakers': every
           AirPlay, Bluetooth, USB, HDMI or other output, by name, each with a
           small icon for its kind (in color while it plays).
        5. Click a speaker's round icon to start streaming to it. Click it again to turn
           it off. Turning streaming off keeps your speakers chosen: their icons
           turn outlined, and they play again when you turn streaming on or click
           any speaker.

        More Controls
        - Sync: the Mac's built-in speakers are the reference and have no delay.
          Every other local speaker has a delay relative to them, which always
          applies: click the time next to it to adjust it. A positive value
          holds the speaker back; a negative one is for a speaker that sounds
          late, such as Bluetooth. While an AirPlay speaker is on, every local
          speaker also waits the 'AirPlay delay' under Connected Speakers for it.
        - Groups: turn on the speakers you want, then choose 'New Group from Active
          Speakers' in the gear menu. Click the group to switch them all at once.
          To rename or delete a group, right-click it or use its ⋯ button.
        - Auto-connect: right-click a speaker, or use its ⋯ button, and turn on
          'Auto-connect when available'. AudioSync then turns it on whenever it
          appears.
        - Rename: double-click a speaker's name, or choose 'Rename…' from its
          ⋯ button, type a name and press Return. Leave it empty to go back to
          the device's own name, which its tooltip keeps showing.
        - Equalizer: click 'Equalizer' in the window for 10 bands, a preamp and
          presets. It shapes the sound on every speaker at once.
        - Now Playing: while Spotify or Music is open with a track, the window
          shows it with play/pause and skip, and AirPlay speakers with a
          screen, such as a TV, show its title and artwork. Other apps aren't
          shown: macOS doesn't let apps read the system-wide Now Playing info.
          macOS asks once for Automation access to Spotify or Music.
        - Virtual devices (loopback drivers, meeting apps, aggregate devices) are
          hidden unless you use them; 'Show Virtual Devices' in the gear menu
          lists them all.
        - Launch at Login: in the gear menu, or in the AudioSync menu. The window
          doesn't open at login; click the Dock icon to bring it back.
        - Appearance: in the gear menu, or in the AudioSync menu. Match System
          follows macOS; Light or Dark keeps AudioSync's windows that way.
        - When the source app quits, streaming stops. To keep playing all system
          audio instead, turn on 'Switch to All System Audio When the Source App
          Quits' in the gear menu or the AudioSync menu.

        Speaker Notes
        \(guideCodeNote)

        AirPlay
        AudioSync sends to AirPlay speakers itself. If its AirPlay sender stops
        after an internal error, Connected Speakers says so, with a Retry button.

        Notices
        When something needs your attention, such as a speaker that was switched
        off or a source that quit, a notice appears at the top of the window and
        of the menu bar panel, and a dot appears next to the menu bar icon.
        Several can wait at once. Click × to dismiss one, or 'Dismiss All'
        when there is more than one. A notice also clears itself once its
        cause is gone.

        Tips
        - Tune with just two speakers at first: one AirPlay and one local.
        - Pick a track with sharp sounds, like drums, claps or speech, so any
          offset is easy to hear.
        - Line the built-in speakers up with the HomePod first: if the Mac plays
          first and the HomePod echoes behind it, raise the AirPlay delay; if the
          HomePod plays first, lower it.
        - Then tune each other local speaker against the built-in speakers: if it
          plays first, raise its delay; if it plays late, lower it.
        - Bluetooth speakers and headphones add their own lag; they usually need
          about −150 to −300 ms.
        - Add multiple local speakers one by one and tune each separately.

        Volume
        Local speakers' sliders follow how loudness is heard: halfway is about −18 dB.
        AirPlay speakers use the AirPlay volume scale.

        Notes
        - AirPlay speakers are kept in step with each other by AudioSync's AirPlay
          sender, so they need no delay.
        - Total latency is about 2 seconds because of the AirPlay buffer.
        - Delay, volume, and selection are saved per device and restored automatically.
        """
    }

    /// The Guide's note on 'Asks for a code'.
    private var guideCodeNote: String {
        let asks = """
            - 'Asks for a code': the speaker wants a PIN or password. AudioSync asks
              for it only when you turn the speaker on yourself. If it was turned on
              automatically (auto-connect, or at launch), it is switched off with a
              notice instead.
            """
        return asks + """
             An Apple TV pairs once, with the PIN it
              shows; a password is asked for once each time AudioSync starts. If a
              code doesn't work, AudioSync asks again; if you cancel, the speaker
              turns off. Turn it on again to retry.
            """
    }
}
