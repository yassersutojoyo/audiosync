# Edge cases

What AudioSync does when the world changes under a running session, where the code that handles it lives, and how each case is checked. Cite a case by its ID (EC1 to EC12); the IDs don't change when cases are added.

"Automated" names the checks in `Tests/` that cover the decision logic; every logic check for a case starts with its ID, so `Tests/run-tests.sh | grep EC7` lists them. Ring-buffer checks are cited by name too, but the test run only prints the ring suite's summary, so their names show up only when one fails. Anything that needs a person (unplugging a device, Bluetooth, sleep, changing a device's format, the network) is under "Manual". The manual steps play audio, so do them at a sensible volume.

| ID | Case | Checked by |
|---|---|---|
| EC1 | Device unplugged and replugged during playback | Automated (logic) and manual |
| EC2 | Bluetooth disconnect and reconnect | Automated (logic) and manual |
| EC3 | Sleep and wake | Automated (logic, ring buffer, FIFO) and manual |
| EC4 | Sample-rate change on an output | Automated (logic, ring buffer) and manual |
| EC5 | Switching the source during playback | Automated (logic, ring buffer, FIFO) and manual |
| EC6 | The source app quits | Automated (logic) and manual |
| EC7 | Engine restart (OwnTone killed) | Automated (logic, FIFO) and manual |
| EC8 | Every speaker off, then on | Automated (logic) and manual |
| EC9 | The default output changes | Automated (logic) and manual |
| EC10 | Six or more speakers | Automated (logic, ring buffer) and manual |
| EC11 | Very long speaker names | Automated (logic) and manual |
| EC12 | AirPlay network loss | Automated (logic) and manual |

The 3-second poll mentioned below is `AppDelegate.startPolling`, which runs `reconcileAirPlay()` (`AppDelegate+AirPlay.swift`) and `reconcileLocal()` (`AppDelegate+Outputs.swift`).

## EC1: Device unplugged and replugged during playback

Expected: the unplugged speaker goes silent and leaves the list; every other speaker keeps playing and streaming stays on, even when it was the only speaker. The speaker stays switched on, so within one poll of plugging it back in it plays again at its own delay and volume, with no notice. If the unplugged device was the input-device source, streaming stops with the notice "The input device was disconnected, so streaming has stopped."

Code path:
- `OutputRenderer.installDeviceAliveListener` sees `kAudioDevicePropertyDeviceIsAlive` go to 0 and calls `onDeviceLost`; `SyncEngine.handleOutputLost` removes that renderer and reports `onOutputLost` (`AppDelegate.applicationDidFinishLaunching` only logs it; the speaker stays in `enabledLocal`).
- `reconcileLocal()` drops the device from the list while it's gone and lists it again when it returns; `syncLocalOutputs()` uses `LocalOutputSync.plan` (`SpeakerState.swift`) to attach it again by UID (the device's Core Audio ID may change).
- `StreamingPolicy.isIdle` (`SpeakerState.swift`) keeps streaming on while a speaker is on, present or not.
- Input-device source: `ProcessTapCapture.watchInput` sees `kAudioDevicePropertyDeviceIsAlive` go to 0 or the device leave the device list, stops its IOProc and raises `.sourceLost(.inputDisconnected)`; the `engine.onInterrupted` handler in `AppDelegate.swift` stops streaming and posts the `Notice.source` notice.

Automated: "EC1 unplug: a playing speaker that is still present is left alone", "EC1 unplug: a lost speaker stays on and waits while its device is missing", "EC1 unplug: streaming keeps running while the only speaker is unplugged", "EC1 replug: the speaker is attached again once its device is listed", "EC1 switch off: a speaker turned off is detached"; ring buffer "D plays FRESH frames (not replayed old)".

Manual: stream to a USB audio device and one other speaker. Unplug the USB device: the other speaker plays on, the USB row disappears. Plug it back in: within about 3 s the row returns, switched on, and plays in sync again. Repeat with the USB device as the only speaker: streaming stays on while it's unplugged and resumes on replug. With a USB microphone as the source, unplug it: streaming stops with the notice above.

Note: detaching relies on Core Audio's "is alive" notification. A renderer whose device vanished without one would stay attached to the old device ID; `LocalOutputSync.plan` only detaches speakers that were switched off.

## EC2: Bluetooth disconnect and reconnect

Expected: the same as EC1. A headset that drops out (out of range, powered off, taken by another device) goes silent and leaves the list; the rest keep playing. When it reconnects it plays again within one poll. A headset switching to its hands-free profile (another app opens its microphone) changes its sample rate; see EC4.

Code path: the same as EC1 (`OutputRenderer` alive listener, `SyncEngine.handleOutputLost`, `reconcileLocal()`, `syncLocalOutputs()`, `LocalOutputSync.plan`). A Bluetooth headset used as the input-device source follows the input path in EC1. Auto-connect (`AppDelegate+Groups.swift`, `autoConnect`) turns a reappearing speaker on when it's marked for auto-connect.

Automated: "EC2 Bluetooth: a disconnected headset waits; the other speaker keeps playing", "EC2 Bluetooth: a reconnected headset is attached again"; plus the EC1 checks.

Manual: stream to Bluetooth headphones and one other speaker. Turn the headphones off: the other speaker keeps playing, the headphones' row disappears. Turn them on again: the row comes back switched on and plays in sync within a few seconds. Repeat with the headphones marked for auto-connect and switched off in AudioSync before the disconnect: on reconnect they switch on by themselves.

## EC3: Sleep and wake

Expected: nothing plays while the Mac sleeps. After wake, streaming is still on and the local speakers continue with fresh audio at their configured delay, never the audio from before the sleep. AirPlay speakers that were on stay on: the engine keeps running across the sleep, but a speaker's session may not survive it, and one the engine reports as off after wake is selected again and plays, with no notice. Only a speaker that is still off once things have settled, about 20 seconds after the engine first answers after wake, is turned off with the notice "<name> couldn't be reached, so it was turned off." (EC12); one that asks for a PIN or password follows the usual pairing rules. A speaker that was switched off stays off. Devices that disappear across sleep (USB hubs, Bluetooth) follow EC1 and EC2, an AirPlay speaker that left the network follows EC12, and an engine that exits during sleep follows EC7.

Code path:
- `AppDelegate.observeSleepAndWake` (`AppDelegate+AirPlay.swift`) feeds `NSWorkspace.willSleepNotification` and `didWakeNotification` to `AirPlayWake` (`AirPlayReconcile.swift`); `screensDidWakeNotification` counts as a wake too while AudioSync still thinks the Mac is asleep, in case the wake notification goes missing.
- While the Mac sleeps, `reconcileAirPlay()` sends no poll, and an answer or a timeout still on its way is set aside: nothing is selected or dropped, and a failed poll doesn't count as an outage (`AirPlayReachability`).
- On wake, `reconcileAirPlay()` polls at once. `AirPlayWake.polled` starts a grace of `AirPlayWake.grace` (20 s) at the engine's first answer. Within it, `AirPlayReconcile.decide` (`waking`) selects a wanted speaker that needs no code and is reported off again instead of dropping it, at most once per poll. After a failed switch-on, the report the failure asks for at once and a poll that was already waiting behind the switch-on don't select it again; the next poll does (`AirPlayWrites.tooSoonToRetry`). Pipe playback is requested again with every re-select. After the grace the usual rules apply (EC12). A new engine run is recognized as in EC7.
- Core Audio stops and resumes the capture IOProc (`ProcessTapCapture.IOLink`, on the tap's aggregate device or the input device) and the renderers; `DelayReader` (`RingBuffer.swift`) plays silence while nothing arrives and re-establishes the delay without replaying old audio. `FifoFeed`'s writer keeps retrying its pipe.

Automated: "EC3 sleep: a poll that comes back while the Mac sleeps is set aside", "EC3 wake: a speaker reported off right after wake is selected again, not dropped", "EC3 wake: a speaker still on at the first answer and off at the next is selected again", "EC3 wake: a switch-on that fails after wake is tried again at the next poll, not at once", "EC3 wake: a speaker switched off before the sleep stays off", "EC3 wake: a speaker still off once the grace is over is turned off with a notice", "EC3 wake: the grace starts at the engine's first answer after wake", "EC3 wake: a speaker that asks for a code after wake isn't switched on again"; ring buffer "C underrun eventually emits silence", "C no replay loop (stays silent once drained)", "H stall: never replays audio", "H stall: resumes after the last frame it had", "H stall: delay restored"; FIFO "fifo: reader closing mid-stream does not kill the process".

Manual: start AudioSync with the debug log on (`docs/DEVELOPMENT.md`). Stream to a local speaker and two AirPlay speakers with some music playing, then switch one AirPlay speaker off. Put the Mac to sleep from the Apple menu, wait at least five minutes, wake it. Expected: streaming is still on, the local speaker resumes with fresh audio, the AirPlay speaker that was on plays again in sync within about 20 seconds, still switched on and with no notice, and the other one stays off. No old audio is replayed. The debug log shows "going to sleep" and "woke from sleep". Repeat once with a long sleep (lid closed for an hour or overnight, on power so Power Nap can run): same result. Then unplug the AirPlay speaker while the Mac sleeps and wake it: while the engine still lists it, it is turned off with the notice above once the grace is over; if the engine no longer lists it, it leaves the list and stays switched on (EC12). Plugged back in, it plays again, or after switching it on if it was turned off. If a speaker that was on is turned off with a notice after a normal wake, attach the debug log.

## EC4: Sample-rate change on an output

Expected: a local speaker whose device changes rate keeps playing, at most with a short glitch, and stays in sync. When the change also changes the capture rate (the tap's format follows the output; an input-device source can be switched to another rate), streaming rebuilds itself at the new rate: a short dropout on every speaker, then playback resumes with the same delays and no notice.

Code path:
- Each `OutputRenderer.start` gives its output unit the capture rate as the input format, so the output unit converts to whatever rate its device runs at.
- `ProcessTapCapture` listens to `kAudioTapPropertyFormat` and the aggregate device's `kAudioDevicePropertyNominalSampleRate` (tap), or the input device's `kAudioDevicePropertyNominalSampleRate`; `CaptureRate.running` takes the rate of the device the IOProc runs on (the tap's own rate only when that can't be read) and `CaptureRate.needsRestart` (`CaptureCheck.swift`) decides; an app tap built again for new processes goes through the same rule; `.formatChanged` goes through `SyncEngine.handleInterruption` to the `engine.onInterrupted` handler in `AppDelegate.swift`, which calls `startEngine(origin: .automatic)`.
- Delays are kept in milliseconds and converted at the new rate (`OutputRenderer.setDelayMs`); `FifoFeed.setInputRate` resamples to the engine's 48 kHz.

Automated: "EC4 rate: a new capture rate rebuilds the session", "EC4 rate: a higher capture rate rebuilds the session", "EC4 rate: the same rate keeps playing", "EC4 rate: an unreadable rate keeps playing"; logic "capture rate: …", "fifo: <rate> Hz → 48 kHz frame count"; ring buffer "I …: lag stays within tolerance".

Manual: stream to a USB output and an AirPlay speaker. In Audio MIDI Setup, change the USB output's format from 48,000 Hz to 44,100 Hz and back. Expected: at most a short glitch, then in sync with AirPlay, no notice. Do the same on the Mac's default output device: a short dropout everywhere, then playback resumes in sync; the debug log shows "capture rate changed".

## EC5: Switching the source during playback

Expected: choosing another source while streaming gives a short gap, then only the new source plays on every speaker that was on. AirPlay speakers stay selected; nothing from the old source is replayed. If the new source can't start (an input device while microphone access is off, or an app that quit between the click and the start), streaming stops with an alert and a notice. While streaming is off, a new source is only remembered for the next start.

Code path: `AppDelegate.setSource` (`AppDelegate+Outputs.swift`) asks `SourceSwitch.decide` (`AudioSource.swift`); `.restart` calls `stopEngine(deselectAirPlay: false)` and `startEngine(origin: .user)` (`AppDelegate+Engine.swift`). `SyncEngine.stop` resets the ring; `ProcessTapCapture.start` builds a tap for the new source.

Automated: "EC5 source: switching while streaming rebuilds the capture", "EC5 source: switching to an input device while streaming rebuilds the capture", "EC5 source: switching while stopped only remembers the choice", "EC5 source: choosing the same source changes nothing"; logic "ring: reset clears history"; FIFO "fifo: stop/start leaves one writer thread".

Manual: stream the whole system with music in one app and a video in a browser. Switch the source to the music app: after a short gap only the music plays, AirPlay speakers stay on. To see the failure path, turn off AudioSync under System Settings → Privacy & Security → Microphone, then switch the source to an input device: an alert says so, a notice stays, and streaming stops. (The source list only offers running apps, so a not-running app can't be picked by hand.)

## EC6: The source app quits

Expected: when the app chosen as the source quits while streaming, streaming stops, the AirPlay speakers are released, and the notice "<App> quit, so streaming has stopped." appears. The speakers stay switched on (shown as waiting) and the source stays that app, so pressing Start after reopening it streams again. Quitting any other app changes nothing; the whole-system and input-device sources never stop because an app quits.

Code path: the `NSWorkspace.didTerminateApplicationNotification` observer in `AppDelegate.applicationDidFinishLaunching` asks `AudioSource.endsWhenAppQuits` (`AudioSource.swift`), then calls `stopEngine(deselectAirPlay: true)` and posts `Notice.source`. `ProcessTapCapture.followApp` may see the app's audio processes vanish first (`AppTapFollow`, `CaptureSupport.swift`): while the app still runs it drops the tap and waits for a new audio process; once the app is gone, or if it can't build the new tap, it raises `.sourceLost`, which ends in the same stop and the same notice subject, so only one notice shows.

Automated: "EC6 app quit: the source app quitting ends the session", "EC6 app quit: another app quitting changes nothing", "EC6 app quit: the whole-system source never ends on a quit", "EC6 app quit: an input source never ends on a quit", "EC6 app quit: an app without a bundle ID changes nothing"; logic "app tap: …" (the tap follows the app's audio processes, waits while there are none and loses the source once the app is gone).

Manual: choose Music as the source and stream it. Quit Music: streaming stops with the notice above, the speakers show as waiting. Reopen Music, press play and Start: streaming resumes. With the whole system as the source, quit an app that is playing: streaming continues.

## EC7: Engine restart (OwnTone killed)

Expected: AirPlay goes quiet while the speaker list stays as it was. About two seconds later a new engine starts, and on its first answer (or, if it hasn't found them yet, as soon as it lists them) every AirPlay speaker that was on is selected again and plays, with no notice. Every one of them, however many and whenever the new engine finds them, plays at its own volume (its slider's, times the master): a speaker AudioSync switches on starts at it, and one the new engine switches on by itself, from its database, gets it back at the first answer that lists it. The volume the new engine holds for a speaker (stale after a kill, or its master volume, often 100, for a speaker it never saved) is never kept. AirPlay comes back with the latency it had before the restart, so the local speakers still line up with it at their saved delays. Local speakers keep playing throughout. After more than five starts within a minute the engine gives up: the AirPlay section says "The AirPlay engine stopped after repeated crashes." and offers Retry.

Code path:
- `AirPlayEngine.launch` sets the process `terminationHandler`, which reports the exit to `AirPlayEngineLifecycle.exited` (`EngineLifecycle.swift`) and starts again after 2 s; `AirPlayEngineLifecycle.willLaunch` is the crash-loop guard (`maxStarts` within `window`).
- `reconcileAirPlay()` keeps the list while `OwnToneClient.airplayOutputs` gets no answer (`AirPlayReconcile.reachable`). Each poll goes to one engine run (`AirPlayEngine.run`, counted on every start, with its own API password), so `AirPlayReachability` (`AirPlayReconcile.swift`) recognizes the first answer from the new engine even when the restart fell between two polls and none of them failed. That answer sets `engineCameBack`: `AirPlayReconcile.decide` selects every wanted speaker, and pipe playback is restored. A new engine can answer before it has found the speakers again: those it lists later are selected as they appear (`AirPlayReachability.restoresSpeakerVolumes` marks the first answer that lists one).
- Volumes (`AirPlayVolume` in `AirPlayReconcile.swift`): AudioSync keeps each AirPlay speaker's own volume (`airplayVolume`, its slider, saved across launches) and sends the engine per-speaker volumes only, each the speaker's own times the master slider (`engineVolume`). It never writes the engine's master volume, not at a start, a restart or a master slider move: the engine's master is its loudest selected speaker's volume, so writing AudioSync's master position would set a lone speaker to it (master 100 put the speaker at full volume). Every switch-on carries the volume, once before the selection and once with it (`selectRequests`), and a start switches every wanted speaker on that way (`started`). A start and a poll send every volume before any switch-on (`ordered`): a switch-on holds the engine's write queue for up to 10 s, while a speaker the engine already plays keeps the engine's volume until its own request gets through. After a restart `polled` sends each wanted speaker the new engine reports on its volume at the first answer that lists it: the restore answer, or the answer where a speaker found later first appears. A reported volume never replaces a speaker's own (only a speaker without one takes the engine's, at most 60); a playing speaker the engine reports at another volume gets AudioSync's again (`trusts`). No poll sends a volume to a speaker AudioSync doesn't stream to, and a report never replaces an own volume it already has, so one the engine switches on by itself while AudioSync is stopped (its database still has it on after a crash or a SIGTERM) starts at its own volume at the next start.
- `FifoFeed`'s writer survives the reader going away (no SIGPIPE) and reopens the pipe for the new engine at the live edge, as a fresh start does: the audio queued while no engine read the pipe is dropped (and counted as a backlog skip), not played late, so the AirPlay leg doesn't get later with each restart.

Automated: "EC7 engine restart: every wanted speaker is selected again, the rest stay off", "EC7 engine restart: a killed engine counts as unreachable", "EC7 engine restart: a killed engine is started again", "EC7 engine restart: a restart with no failed poll still re-selects the speaker", "EC7 engine restart: the next answer from the same engine is not another restart", "EC7 engine restart: an answer after a failed poll counts as a restart", "EC7 engine restart: a restart during a switch-on still counts as a restart", "EC7 engine restart: the volumes are restored when the wanted speakers appear after the first answer", "EC7 engine restart: a restart while stopped leaves the volumes to the next start", "EC7 engine restart: switching a speaker on sends its own volume before and with the selection", "EC7 engine restart: a restarted engine's volume doesn't replace the speaker's own", "EC7 engine restart: a speaker the new engine plays gets its own volume times the master", "EC7 engine restart: a speaker found after the restore answer gets its own volume, never the engine's", "EC7 engine restart: a speaker the engine switched on by itself while stopped keeps its own volume"; logic "volume: a start switches each wanted speaker on at its own volume times the master", "volume: moving the master scales each playing speaker from its own volume, never above it", "volume: after a master move the poll keeps the speaker's own volume", "volume: a speaker AudioSync doesn't want on never has its volume taken", "engine: exit → restarting", "engine: six exits in a minute → gaveUp", "engine: gaveUp offers Retry", "engine: occasional exits never give up", "reconcile: after an engine restart the speaker is selected again", "reconcile: a speaker with a switch-on in flight isn't re-selected after an engine restart"; FIFO "fifo: reader closing mid-stream does not kill the process", "EC7 engine restart: the reopened pipe starts as close to live as a fresh start".

Manual: stream to an AirPlay speaker and a local speaker, with the local delay set so the two line up. In Terminal, run `pkill -KILL -f 'AudioSync.app/Contents/Helpers/owntone'` (a plain `pkill` sends SIGTERM, which the engine treats as a clean stop: it saves and restores its speakers itself, so the crash path isn't exercised). Expected: the local speaker keeps playing; the AirPlay speaker goes quiet and plays again within about 10 s, still switched on, at its own volume times the master, from the engine's first answer, no notice, and still in step with the local speaker at the same delay. Run the command six times within a minute (wait for each restart): the give-up message and Retry appear; Retry brings AirPlay back.

## EC8: Every speaker off, then on

Expected: switching off the last speaker stops streaming and releases the AirPlay speakers; the source app's own audio is no longer muted. Switching any speaker on again starts streaming to it, with fresh audio only. Pressing Start with no speaker on shows "Turn on at least one speaker first (under This Mac or Connected Speakers)."

Code path: `setLocalEnabled` and `setAirPlayEnabled` (`AppDelegate+Outputs.swift`) start streaming through `StreamingPolicy.startsStreaming` and stop it through `stopIfIdle()` with `StreamingPolicy.isIdle` (`SpeakerState.swift`); `startEngine` checks `StreamingPolicy.hasSpeaker` first. `stopEngine(deselectAirPlay: true)` and `SyncEngine.stop` (ring reset) end the session.

Automated: "EC8 all off: streaming stops when the last speaker is switched off", "EC8 all off: one AirPlay speaker still on keeps streaming", "EC8 all off: nothing to stop while stopped", "EC8 all off: turning a speaker on again starts streaming", "EC8 all off: only a switch-on while stopped starts streaming", "EC8 all off: Start with no speaker on asks for one first"; logic "speaker: click on a waiting speaker turns it on (and starts streaming)", "ring: reset clears history".

Manual: stream to two speakers. Switch both off one by one: streaming stops after the second, the source app is audible on the Mac's own output again. Switch one on: streaming starts on that speaker alone, without a burst of old audio.

## EC9: The default output changes

Expected: changing the Mac's sound output (Control Center or System Settings) while streaming changes nothing in AudioSync: every chosen speaker keeps playing, the source stays muted on the new default device (no doubled audio), and the speaker list doesn't change. If the new default device runs at a different rate, the capture rebuilds as in EC4. While streaming is off, other apps simply play on the new default.

Code path: AudioSync doesn't follow the default output. Each `OutputRenderer` is bound to its own device ID; the tap built in `ProcessTapCapture.TapObjects` captures processes, not a device, and mutes their own playback wherever it goes (`muteBehavior = .muted`). A format change on the tap goes through `CaptureRate.needsRestart` (EC4). `AudioDevices.defaultOutputDevice()` exists but nothing in the session uses it.

Automated: the EC4 checks ("EC4 rate: …"). Nothing else here is decision logic; the rest is manual.

Manual: stream the whole system to an AirPlay speaker and a USB speaker, with the Mac's built-in speakers as the default output. Switch the default output to the USB speaker, then to headphones, then back. Expected: no interruption longer than a brief rebuild, nothing doubled on the new default device, the speaker rows unchanged.

## EC10: Six or more speakers

Expected: every speaker is listed, This Mac first and the rest by name; all of them can be on at once and play in sync. The menu bar panel scrolls once its rows don't fit (it shows at least six rows); the main window scrolls too.

Code path: `SpeakerLayout.split` (`SpeakerLayout.swift`) orders the rows; the menu bar panel switches to a `ScrollView` above `maxListHeight` (`MenuBar.swift`). Each local speaker gets its own `OutputRenderer` and `DelayReader` on the shared ring (`SyncEngine.addOutput`); all AirPlay speakers share one engine and are reconciled per speaker (`AirPlayReconcile.decide`).

Automated: "EC10 many speakers: sixteen speakers all listed, in name order", "EC10 many speakers: eight AirPlay speakers are selected together, in the engine's order", "EC10 many speakers: eight local speakers are attached together"; ring buffer "F readers independent (different positions)".

Manual: with six or more speakers available (AirPlay speakers, or local and virtual devices with Show Virtual Devices on), switch them all on. Expected: the panel scrolls and every row is reachable, all speakers play in sync, AudioSync's CPU use stays modest in Activity Monitor.

## EC11: Very long speaker names

Expected: a long name is cut to one line with an ellipsis; the type icon, availability note and switch stay whole and aligned, and the window and panel don't grow sideways. Names are sorted and stored in full, including emoji and non-ASCII characters. In the menu bar panel, a notice that contains a long name wraps to at most three lines while several notices show, with the full text in the tooltip.

Code path: the speaker rows in `MainWindow.swift` and `MenuBar.swift` use `lineLimit(1)`; `SpeakerTypeIcon` has `layoutPriority(1)` so the name truncates first. `SpeakerLayout.split` sorts the full names; `OwnToneOutputs.parse` reads the engine's names unchanged.

Automated: "EC11 long names: names that differ only at the end still sort and stay whole", "EC11 long names: the engine's long, non-ASCII name is read unchanged".

Manual: create an aggregate device in Audio MIDI Setup with a 100-character name (or rename an AirPlay speaker in the Home app), turn on Show Virtual Devices if needed, and look at the main window and the menu bar panel. Expected: one line with an ellipsis, icons and switches visible, no sideways scrolling. Delete the test device afterwards.

## EC12: AirPlay network loss

Expected: when an AirPlay speaker drops off the network, it disappears from the list but stays switched on; the local speakers stop waiting for AirPlay and play without their delay while no chosen AirPlay speaker is listed. When it comes back it is selected again and plays, and the local delays return. If the engine still lists the speaker but reports it off (its session failed), it is turned off with the notice "<name> couldn't be reached, so it was turned off." If no AirPlay speaker is found at all, a Local Network hint shows only when that permission is denied, or unknown for 20 s.

Code path: `reconcileAirPlay()` (`AppDelegate+AirPlay.swift`) rebuilds `airplayOrder` from each poll; `AirPlayReconcile.decide` (`AirPlayReconcile.swift`) only acts on listed speakers (`appeared` gives `.select`, a listed but dropped speaker goes through `AirPlayPairing.dropped` to `.drop(.unreachable)`, handled by `turnOffDropped`). `LocalDelay.airPlayChosen` (`SpeakerState.swift`) and `refreshLocalDelays()` (`AppDelegate+Outputs.swift`) set the local delays. `AirPlayReconcile.localNetworkHint` decides the hint.

Automated: "EC12 network loss: a speaker missing from the engine's list stays on, with nothing to do", "EC12 network loss: local speakers stop waiting for an AirPlay speaker that is gone", "EC12 network loss: a speaker back in the list is selected again", "EC12 network loss: a listed speaker the engine dropped is turned off with a notice", "EC12 network loss: an empty list with Local Network allowed shows no permission hint"; logic "delay: a chosen speaker not listed today doesn't count", "reconcile: a wanted speaker that just appeared is selected", "reconcile: wanted, nothing pending, engine says off → dropped as unreachable".

Manual: stream to an AirPlay speaker and a local speaker. Unplug the AirPlay speaker's power (or turn off the Mac's Wi-Fi). Expected: the local speaker keeps playing, a moment later without its delay; the AirPlay row disappears without a notice, or is turned off with the notice above. Power the speaker back on (or turn Wi-Fi on): if it disappeared, it returns switched on and plays again in sync; if it was turned off, switching it on plays again.
