# Edge cases

What AudioSync does when the world changes under a running session, where the code that handles it lives, and how each case is checked. Cite a case by its ID (EC1 to EC13); the IDs don't change when cases are added.

"Automated" names the checks in `Tests/` that cover the decision logic; every logic check for a case starts with its ID, so `Tests/run-tests.sh | grep EC7` lists them. Ring-buffer checks are cited by name too, but the test run only prints the ring suite's summary, so their names show up only when one fails. Anything that needs a person (unplugging a device, Bluetooth, sleep, changing a device's format, the network) is under "Manual". The manual steps play audio, so do them at a sensible volume.

| ID | Case | Checked by |
|---|---|---|
| EC1 | Device unplugged and replugged during playback | Automated (logic) and manual |
| EC2 | Bluetooth disconnect and reconnect | Automated (logic) and manual |
| EC3 | Sleep and wake | Automated (core) and manual |
| EC4 | Sample-rate change on an output | Automated (logic, core) and manual |
| EC5 | Switching the source during playback | Automated (logic, core) and manual |
| EC6 | The source app quits | Automated (logic) and manual |
| EC7 | The AirPlay sender stops after an internal error | Automated (logic) and manual |
| EC8 | Every speaker off, then on | Automated (logic) and manual |
| EC9 | The default output changes | Automated (logic) and manual |
| EC10 | Six or more speakers | Automated (logic, core) and manual |
| EC11 | Very long speaker names | Automated (logic) and manual |
| EC12 | AirPlay network loss | Automated (logic) and manual |
| EC13 | Streaming switched off and on while audio plays | Automated (logic) and manual |

The 3-second poll mentioned below is `AppDelegate.startReconcileTimer`, which runs `reconcileAirPlaySpeakers()` (`AppDelegate+AirPlay.swift`) and `reconcileLocal()` (`AppDelegate+Outputs.swift`).

## EC1: Device unplugged and replugged during playback

Expected: the unplugged speaker goes silent and leaves the list; every other speaker keeps playing and streaming stays on, even when it was the only speaker. The speaker stays switched on, so within one poll of plugging it back in it plays again at its own delay and volume, with no notice. If the unplugged device was the input-device source, streaming stops with the notice "The input device was disconnected, so streaming has stopped."

Code path:
- `OutputRenderer.installDeviceAliveListener` sees `kAudioDevicePropertyDeviceIsAlive` go to 0 and calls `onDeviceLost`; `SyncEngine.handleOutputLost` removes that renderer and reports `onOutputLost` (`AppDelegate.handleEngineEvents` only logs it; the speaker stays in `enabledLocal`).
- `reconcileLocal()` drops the device from the list while it's gone and lists it again when it returns; `syncLocalOutputs()` uses `LocalOutputSync.plan` (`SpeakerState.swift`) to attach it again by UID (the device's Core Audio ID may change).
- `StreamingPolicy.isIdle` (`SpeakerState.swift`) keeps streaming on while a speaker is on, present or not.
- Input-device source: `SourceCapture.watchInput` sees `kAudioDevicePropertyDeviceIsAlive` go to 0 or the device leave the device list, stops its IOProc and raises `.sourceLost(.inputDisconnected)`; the `engine.onInterrupted` handler in `AppDelegate.swift` stops streaming and posts the `Notice.source` notice.

Automated: "EC1 unplug: a playing speaker that is still present is left alone", "EC1 unplug: a lost speaker stays on and waits while its device is missing", "EC1 unplug: streaming keeps running while the only speaker is unplugged", "EC1 replug: the speaker is attached again once its device is listed", "EC1 switch off: a speaker turned off is detached"; core `a_replugged_output_never_replays_what_it_played_before` (`core/src/reader.rs`).

Manual: stream to a USB audio device and one other speaker. Unplug the USB device: the other speaker plays on, the USB row disappears. Plug it back in: within about 3 s the row returns, switched on, and plays in sync again. Repeat with the USB device as the only speaker: streaming stays on while it's unplugged and resumes on replug. With a USB microphone as the source, unplug it: streaming stops with the notice above.

Note: detaching relies on Core Audio's "is alive" notification. A renderer whose device vanished without one would stay attached to the old device ID; `LocalOutputSync.plan` only detaches speakers that were switched off.

## EC2: Bluetooth disconnect and reconnect

Expected: the same as EC1. A headset that drops out (out of range, powered off, taken by another device) goes silent and leaves the list; the rest keep playing. When it reconnects it plays again within one poll. A headset switching to its hands-free profile (another app opens its microphone) changes its sample rate; see EC4.

Code path: the same as EC1 (`OutputRenderer` alive listener, `SyncEngine.handleOutputLost`, `reconcileLocal()`, `syncLocalOutputs()`, `LocalOutputSync.plan`). A Bluetooth headset used as the input-device source follows the input path in EC1. Auto-connect (`AppDelegate+Groups.swift`, `autoConnect`) turns a reappearing speaker on when it's marked for auto-connect.

Automated: "EC2 Bluetooth: a disconnected headset waits; the other speaker keeps playing", "EC2 Bluetooth: a reconnected headset is attached again"; plus the EC1 checks.

Manual: stream to Bluetooth headphones and one other speaker. Turn the headphones off: the other speaker keeps playing, the headphones' row disappears. Turn them on again: the row comes back switched on and plays in sync within a few seconds. Repeat with the headphones marked for auto-connect and switched off in AudioSync before the disconnect: on reconnect they switch on by themselves.

## EC3: Sleep and wake

Expected: nothing plays while the Mac sleeps. After wake, streaming is still on and the local speakers continue with fresh audio at their configured delay, never the audio from before the sleep. AirPlay speakers that were on stay on, with no notice: a speaker whose session didn't survive the sleep gets a new one from the sender by itself (after 1, 2, 4, 8 and 16 s, then every 30 s) until it plays. A speaker that was switched off stays off. Devices that disappear across sleep (USB hubs, Bluetooth) follow EC1 and EC2, and an AirPlay speaker that left the network follows EC12.

Code path:
- `AppDelegate.observeSleepAndWake` (`AppDelegate+AirPlay.swift`) sets `macAsleep` on `NSWorkspace.willSleepNotification` and clears it on `didWakeNotification`; `screensDidWakeNotification` counts as a wake too while AudioSync still thinks the Mac is asleep, in case the wake notification goes missing. While the Mac sleeps, `reconcileAirPlaySpeakers()` asks the sender nothing; on wake it runs at once, so the sender's speakers match the switches again (`syncNativeSpeakers`).
- Core Audio stops and resumes the capture IOProc (`SourceCapture.IOLink`, on the tap's aggregate device or the input device) and the renderers; the core's delay reader (`CoreDelayReader`) plays silence while nothing arrives and re-establishes the delay without replaying old audio.

Automated: core `a_writer_stall_above_the_dead_band_holds_silence`, `a_reader_that_ran_dry_plays_new_frames_as_they_arrive` and `stall_resumes_after_the_last_frame_at_the_exact_delay` (`core/src/reader.rs`).

Manual: start AudioSync with the debug log on (`docs/DEVELOPMENT.md`). Stream to a local speaker and two AirPlay speakers with some music playing, then switch one AirPlay speaker off. Put the Mac to sleep from the Apple menu, wait at least five minutes, wake it. Expected: streaming is still on, the local speaker resumes with fresh audio, the AirPlay speaker that was on plays again in sync within about half a minute, still switched on and with no notice, and the other one stays off. In sync means at the same delay setting as before the sleep, with no extra lag on the AirPlay speaker. No old audio is replayed. The debug log shows "going to sleep" and "woke from sleep". Repeat once with a long sleep (lid closed for an hour or overnight, on power so Power Nap can run): same result. Then unplug the AirPlay speaker while the Mac sleeps and wake it: it leaves the list and stays switched on (EC12); plugged back in, it plays again. If a speaker that was on is turned off with a notice after a normal wake, attach the debug log.

## EC4: Sample-rate change on an output

Expected: a local speaker whose device changes rate keeps playing, at most with a short glitch, and stays in sync. When the change also changes the capture rate (the tap's format follows the output; an input-device source can be switched to another rate), streaming rebuilds itself at the new rate: a short dropout on every speaker, then playback resumes with the same delays and no notice.

Code path:
- Each `OutputRenderer.start` gives its output unit the capture rate as the input format, so the output unit converts to whatever rate its device runs at.
- `SourceCapture` listens to `kAudioTapPropertyFormat` and the aggregate device's `kAudioDevicePropertyNominalSampleRate` (tap), or the input device's `kAudioDevicePropertyNominalSampleRate`; `CaptureRate.running` takes the rate of the device the IOProc runs on (the tap's own rate only when that can't be read) and `CaptureRate.needsRestart` (`CaptureCheck.swift`) decides; an app tap built again for new processes goes through the same rule; `.formatChanged` goes through `SyncEngine.handleInterruption` to the `engine.onInterrupted` handler in `AppDelegate.swift`, which calls `startEngine(origin: .automatic)`.
- Delays are kept in milliseconds and converted at the new rate (`OutputRenderer.setDelayMs`); AudioSync's AirPlay sender gets the new rate at the rebuild and resamples to 44.1 kHz itself (`NativeAirPlayTap.prepare`).

Automated: "EC4 rate: a new capture rate rebuilds the session", "EC4 rate: a higher capture rate rebuilds the session", "EC4 rate: the same rate keeps playing", "EC4 rate: an unreadable rate keeps playing", "EC4 rate: a format change rebuilds the session the engine stopped, unless the user switched streaming off"; logic "capture rate: …"; core `core/tests/drift.rs` (drift both ways stays within the tolerance).

Manual: stream to a USB output and an AirPlay speaker. In Audio MIDI Setup, change the USB output's format from 48,000 Hz to 44,100 Hz and back. Expected: at most a short glitch, then in sync with AirPlay, no notice. Do the same on the Mac's default output device: a short dropout everywhere, then playback resumes in sync; the debug log shows "capture rate changed".

## EC5: Switching the source during playback

Expected: choosing another source while streaming gives a short gap, then only the new source plays on every speaker that was on. AirPlay speakers stay selected; nothing from the old source is replayed. If the new source can't start (an input device while microphone access is off, or an app that quit between the click and the start), streaming stops with an alert and a notice. While streaming is off, a new source is only remembered for the next start.

Code path: `AppDelegate.setSource` (`AppDelegate+Outputs.swift`) asks `SourceSwitch.decide` (`AudioSource.swift`); `.restart` calls `stopEngine()` and `startEngine(origin: .user)` (`AppDelegate+Engine.swift`). `SyncEngine.stop` resets the ring; `SourceCapture.halt` stops and destroys the capture IOProc at once, and `SourceCapture.start` builds a tap for the new source. The old tap and its aggregate device are destroyed on the main queue's next turn, after the new tap exists (`CaptureClose`, `CaptureSupport.swift`): a tap made right after the old one was destroyed failed, and the old tap keeps the old source muted until the new one takes over.

Automated: "EC5 source: switching while streaming rebuilds the capture", "EC5 source: switching to an input device while streaming rebuilds the capture", "EC5 source: switching while stopped only remembers the choice", "EC5 source: choosing the same source changes nothing"; logic "capture close: a source switch makes the new tap before the old tap is destroyed"; every session gets a new core ring (`EngineParts.make`).

Manual: stream the whole system with music in one app and a video in a browser. Switch the source to the music app: after a short gap only the music plays, AirPlay speakers stay on. To see the failure path, turn off AudioSync under System Settings → Privacy & Security → Microphone, then switch the source to an input device: an alert says so, a notice stays, and streaming stops. (The source list only offers running apps, so a not-running app can't be picked by hand.)

## EC6: The source app quits

Expected: when the app chosen as the source quits while streaming, streaming stops, the AirPlay speakers are released, and the notice "<App> quit, so streaming has stopped." appears. The speakers stay switched on (shown as waiting) and the source stays that app, so pressing Start after reopening it streams again. Quitting any other app changes nothing; the whole-system and input-device sources never stop because an app quits.

Code path: the `NSWorkspace.didTerminateApplicationNotification` observer (`AppDelegate.observeApps`) asks `AppLifecycle.endsSession` (`AppLifecycle.swift`), which asks `AudioSource.endsWhenAppQuits` (`AudioSource.swift`) while streaming, then calls `stopEngine(deselectAirPlay: true)` and posts `Notice.source`. `SourceCapture.followApp` may see the app's audio processes vanish first (`AppTapFollow`, `CaptureSupport.swift`): while the app still runs it drops the tap and waits for a new audio process (when only some of them change, `AppRetap` changes the running tap's description in place, or builds the new tap before closing the old one, so the app never plays unmuted); once the app is gone, or if it can't build the new tap, it raises `.sourceLost`, which ends in the same stop and the same notice subject, so only one notice shows.

Automated: "EC6 app quit: the source app quitting ends the session", "EC6 app quit: another app quitting changes nothing", "EC6 app quit: the whole-system source never ends on a quit", "EC6 app quit: an input source never ends on a quit", "EC6 app quit: an app without a bundle ID changes nothing", "EC6 app quit: the source app quitting while streaming stops the session", "EC6 app quit: the source app quitting while stopped changes nothing", "EC6 app quit: another source, or an app without a bundle ID, keeps streaming"; logic "app tap: …" (the tap follows the app's audio processes, waits while there are none and loses the source once the app is gone).

Manual: choose Music as the source and stream it. Quit Music: streaming stops with the notice above, the speakers show as waiting. Reopen Music, press play and Start: streaming resumes. With the whole system as the source, quit an app that is playing: streaming continues.

## EC7: The AirPlay sender stops after an internal error

The sender runs inside the app, so there's no separate process to restart. If it stops after an internal error, its speakers leave the list (their switches stay as they are), the Local Network probe stops, the local speakers stop waiting for AirPlay, and a notice says the sender stopped; the AirPlay section offers Retry. Retry starts a new sender, which finds the speakers again, plays to the ones switched on and gets the current track info sent again. A sender that can't start at all is tried again every 10 s or so, with one notice.

Code path: the core catches a panic at its C interface and reports `ASC_AIRPLAY_EVENT_FAILED` once; `nativeSenderFailed` (`AppDelegate+NativeAirPlay.swift`) drops the sender, and `NativeSenderStarts` (`NativeAirPlayModel.swift`) keeps it from starting again by itself until Retry (`retryNativeAirPlay`). A start that fails is retried by `reconcileNativeAirPlay` after `NativeSenderStarts.retrySpacing`.

Automated: "EC7 native airplay: the sender's failure event makes the app drop it and offer Retry", "EC7 native airplay: a sender that stopped after an internal error doesn't restart by itself", "EC7 native airplay: it shows as failed, with Retry", "EC7 native airplay: Retry starts a new sender at once", "EC7 native airplay: a failed start is told once", "EC7 native airplay: after a failed start, a later reconcile starts it again, not every poll".

Manual: a release build gives no way to make the sender fail on purpose. If the debug log ever shows "native AirPlay: the sender stopped after an internal error", check that the notice and Retry appear, that local speakers keep playing, and that Retry brings the AirPlay speakers back.

## EC8: Every speaker off, then on

Expected: switching off the last speaker stops streaming and releases the AirPlay speakers; the source app's own audio is no longer muted. Switching any speaker on again starts streaming to it, with fresh audio only. Pressing Start with no speaker on shows "Turn on at least one speaker first (under This Mac or Connected Speakers)."

Code path: `setLocalEnabled` (`AppDelegate+Outputs.swift`) and `setNativeAirPlayEnabled` (`AppDelegate+NativeAirPlay.swift`) start streaming through `StreamingPolicy.startsStreaming` and stop it through `stopIfIdle()` with `StreamingPolicy.isIdle` (`SpeakerState.swift`); `startEngine` checks `StreamingPolicy.hasSpeaker` first. `stopEngine()` and `SyncEngine.stop` (ring reset) end the session.

Automated: "EC8 all off: streaming stops when the last speaker is switched off", "EC8 all off: one AirPlay speaker still on keeps streaming", "EC8 all off: nothing to stop while stopped", "EC8 all off: turning a speaker on again starts streaming", "EC8 all off: only a switch-on while stopped starts streaming", "EC8 all off: Start with no speaker on asks for one first"; logic "speaker: click on a waiting speaker turns it on (and starts streaming)"; every session gets a new core ring (`EngineParts.make`).

Manual: stream to two speakers. Switch both off one by one: streaming stops after the second, the source app is audible on the Mac's own output again. Switch one on: streaming starts on that speaker alone, without a burst of old audio.

## EC9: The default output changes

Expected: changing the Mac's sound output (Control Center or System Settings) while streaming changes nothing in AudioSync: every chosen speaker keeps playing, the source stays muted on the new default device (no doubled audio), and the speaker list doesn't change. If the new default device runs at a different rate, the capture rebuilds as in EC4. While streaming is off, other apps simply play on the new default.

Code path: AudioSync doesn't follow the default output. Each `OutputRenderer` is bound to its own device ID; the tap built in `SourceCapture.TapObjects` captures processes, not a device, and mutes their own playback wherever it goes (`muteBehavior = .muted`). A format change on the tap goes through `CaptureRate.needsRestart` (EC4). `AudioDevices.defaultOutputDevice()` exists but nothing in the session uses it.

Automated: the EC4 checks ("EC4 rate: …"). Nothing else here is decision logic; the rest is manual.

Manual: stream the whole system to an AirPlay speaker and a USB speaker, with the Mac's built-in speakers as the default output. Switch the default output to the USB speaker, then to headphones, then back. Expected: no interruption longer than a brief rebuild, nothing doubled on the new default device, the speaker rows unchanged.

## EC10: Six or more speakers

Expected: every speaker is listed, This Mac first and the rest by name; all of them can be on at once and play in sync. The menu bar panel scrolls once its rows don't fit (it shows at least six rows); the main window scrolls too.

Code path: `SpeakerLayout.split` (`SpeakerLayout.swift`) orders the rows; the menu bar panel switches to a `ScrollView` above `maxListHeight` (`MenuBar.swift`). Each local speaker gets its own `OutputRenderer` and `CoreDelayReader` on the shared ring (`SyncEngine.addOutput`); all AirPlay speakers share one sender, which gets each speaker added or removed as it's switched (`syncNativeSpeakers`, `NativeAirPlayModel.plan`).

Automated: "EC10 many speakers: sixteen speakers all listed, in name order", "EC10 many speakers: eight local speakers are attached together"; core `readers_are_independent` (`core/src/reader.rs`) and `readers_do_not_disturb_each_other` (`core/tests/reader_props.rs`); Windows `ec10_eight_local_outputs_switched_on_together_all_play_on_their_own_renderers` (`windows/src/audio/controller.rs`).

Manual: with six or more speakers available (AirPlay speakers, or local and virtual devices with Show Virtual Devices on), switch them all on. Expected: the panel scrolls and every row is reachable, all speakers play in sync, AudioSync's CPU use stays modest in Activity Monitor.

## EC11: Very long speaker names

Expected: a long name is cut to one line with an ellipsis; the type icon, availability note and switch stay whole and aligned, and the window and panel don't grow sideways. Names are sorted and stored in full, including emoji and non-ASCII characters. In the menu bar panel, a notice that contains a long name wraps to at most three lines while several notices show, with the full text in the tooltip.

Code path: the speaker rows in `MainWindow.swift` and `MenuBar.swift` use `lineLimit(1)`; `SpeakerTypeIcon` has `layoutPriority(1)` so the name truncates first. `SpeakerLayout.split` sorts the full names; the AirPlay sender passes receivers' names on unchanged.

Automated: "EC11 long names: names that differ only at the end still sort and stay whole".

Manual: create an aggregate device in Audio MIDI Setup with a 100-character name (or rename an AirPlay speaker in the Home app), turn on Show Virtual Devices if needed, and look at the main window and the menu bar panel. Expected: one line with an ellipsis, icons and switches visible, no sideways scrolling. Delete the test device afterwards.

## EC12: AirPlay network loss

Expected: a speaker whose connection drops or stops answering stays switched on, with no notice, and the sender keeps setting up a new session (after 1, 2, 4, 8 and 16 s, then every 30 s) until it plays again. One that is no longer announced on the network and isn't playing leaves the list, still switched on, and the local speakers stop waiting for AirPlay (the AirPlay delay no longer counts) while no chosen AirPlay speaker is listed; they stay in step with each other by their delays. When it's back it plays again, and the local speakers wait the AirPlay delay for it again. A speaker turned off for good is one that turned the session down for a reason only you can fix (a password, a PIN, a receiver AudioSync can't play to), each with its own notice. If no AirPlay speaker is found at all, a Local Network hint shows only when that permission is denied, or unknown for 20 s.

Code path: `refreshNativeRows` (`AppDelegate+NativeAirPlay.swift`) rebuilds `airplayOrder` from the sender's list (`NativeAirPlayModel.rows`). `NativeSyncRule`, through `airPlayChosenAndListed` (`AppDelegate+Outputs.swift`), says whether an AirPlay speaker is chosen, counting only speakers the sender lists as able to play or playing (`NativeAirPlayModel.soundingIDs`), and `retimeLocalOutputs()` gives the running local speakers their new hold-backs (`LocalDelay.swift`). `LocalNetworkHint.decide` (`LocalNetworkProbe.swift`) decides the hint.

Automated: "EC12 network loss: an empty list with Local Network allowed shows no permission hint", "EC12 native airplay: gone and not playing, it leaves the list and holds nothing back", "EC12 native airplay: a speaker that plays on after its announcement lapsed stays listed and chosen".

Manual: stream to an AirPlay speaker and a local speaker. Unplug the AirPlay speaker's power (or turn off the Mac's Wi-Fi). Expected: the local speaker keeps playing, a moment later without the AirPlay delay once the AirPlay row has disappeared, with no notice. Power the speaker back on (or turn Wi-Fi on): it returns switched on and plays again in sync.

## EC13: Streaming switched off and on while audio plays

Expected: switching streaming off and on again while the source keeps playing, with the streaming switch or by turning the last speaker off and one on again (EC8), brings the AirPlay speakers back in step with the local speakers at their saved delays. An AirPlay speaker sets up a new session after the switch-on, which takes it a second or two, and then plays in step. The same holds when the audio stops for a while with streaming on (the source stalls, or the Mac sleeps, EC3) and comes back.

Code path: `stopEngine` (`AppDelegate+Engine.swift`) stops the session, and `syncNativeSpeakers` takes every speaker out of the sender while nothing streams (`NativeAirPlayModel.plan`). `startEngine` starts a new capture, gives the sender a new timeline (`restartNativeTimeline`, AirPlay spec P143) and adds the switched-on speakers again, each with a new session.

Automated: "EC13 native airplay: a speaker leaves when switched off or when streaming stops".

Manual: start AudioSync with the debug log on (`docs/DEVELOPMENT.md`). Stream to an AirPlay speaker and a local speaker with music playing, the local delay set so the two line up. Switch streaming off, wait 5 s and switch it on while the music keeps playing. Expected: the AirPlay speaker plays again within a few seconds and still lines up with the local speaker at the same delay, without retyping it. Repeat three times, once with streaming off for more than 15 s, then once by switching the last speaker off and on again. The debug log shows the speaker removed at each stop and added again at each start ("native AirPlay: removed …", "native AirPlay: added …").
