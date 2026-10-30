# Changelog

User-visible changes in each version. Loosely follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and [Semantic Versioning](https://semver.org/). Downloads are on the [Releases](https://github.com/yassersutojoyo/audiosync/releases) page.

## [Unreleased]

### Added

- With AirPlay 2 on, several AirPlay 2 speakers play at once, up to 16. A speaker past that limit stays on, and its row says "Waiting: too many AirPlay 2 speakers" until another one stops. On the Mac and on Windows.
- With AirPlay 2 on, a speaker that needs PTP timing while AudioSync plays to AirPlay 2 speakers on another network stays on, its row says "Waiting: AirPlay 2 speakers on another network", and it starts once those speakers are off. On the Mac and on Windows.
- An AirPlay speaker that's on but has left the network waits for it to come back instead of retrying, and its row says "Not on the network right now, plays once it's back". A stereo pair keeps a member that left and names it as missing. On the Mac and on Windows.
- On Windows, AudioSync updates itself. Help > Check for Updates… looks for a newer version, and File > Check for Updates Automatically, on by default, checks at launch, at most once a day after a check that worked. Download and Install fetches the update, checks that it was signed with AudioSync's release key and installs it, and Restart Now opens the new version. If the new version doesn't start within 15 seconds, or Windows security blocks the file, AudioSync puts the old version back and offers the release page, and that version isn't offered again at launch. Builds you make yourself never check. AudioSync 0.7.1 on Windows can't update itself, so get this release from the release page by hand once.
- With AirPlay 2 on, AudioSync remembers per speaker whether it plays with PTP or NTP timing, so the next launch starts with the timing that worked. It's forgotten when the speaker's software changes, when the speaker turns that timing down, and with Forget Pairing. If that saved record is damaged for one speaker, only that speaker loses it and works it out again on its next connect. On the Mac and on Windows.
- A speaker's dimmed volume slider says why it's dimmed: the speaker is off, or a Google Cast speaker isn't playing yet. VoiceOver or Narrator reads the reason, and it shows when you hover over the slider in the main window. On the Mac and on Windows.
- A Google Cast speaker you turned on is on again after you quit and reopen AudioSync, once it shows up on the network, as AirPlay and local speakers already were. A hidden speaker stays off. On the Mac and on Windows.

### Changed

- On Windows, the tray panel slides up and fades in as it opens, as Windows 11's own panels do.
- On Windows, the note that no speakers were found names Windows Firewall only while Windows counts a connected network as Public. On a Private network it asks only that this PC is on the same network as the speakers.
- A Google Cast speaker that ends the stream on its own gets it again, unless it already ended it in the 10 minutes before, which ends the session. Before, the second end in a session always ended it. On the Mac and on Windows.
- When several AirPlay speakers start together, they connect one after another, half a second apart, and a pairing's key exchange no longer holds up the sound of a speaker that's already playing. On the Mac and on Windows.
- On Windows, the main window and the tray panel look more like the Mac app. A speaker's name, kind and note share one line, with the kind as a small icon after the name and the device name in the name's tooltip. Card titles are small grey capitals, a zone's arrow sits at the left with its speakers in one tinted box, the AirPlay delay sits in the Connected Speakers card, and Several Apps sits right under the source. The tray panel lists this PC's speakers under the volume, then zones, then connected speakers.
- A zone is renamed in place, like a speaker, instead of in a dialog. On the Mac, double-click its name in the main window or choose Rename… from its menu: Return keeps the new name, Escape the old one. On Windows, double-click its name, press F2 or choose Rename…, type, then press Enter; a speaker in its row under In zones renames the same way.
- A zone's menu item now says Rename…, as a speaker's does, and a new or renamed zone's name is cut to 60 characters with line breaks turned into spaces, as a speaker name is. On the Mac and on Windows.
- On the Mac, the notice for an AirPlay speaker turned off because it asks for a code now says whether it needs a PIN or a password, as on Windows.
- On the Mac, the notices after a source switch fails or a speaker asks for a PIN or password now say what to do next: pick the source again, turn streaming on again, or, for a speaker the web remote turned on, turn it on again on this Mac.
- AudioSync's AirPlay sending now asks the system for a higher priority. On Windows that's the priority AudioSync's local outputs already had. On the Mac and on Windows.
- A Google Cast speaker that loses its connection tries again after 1, 2, 4 and 8 seconds, then every 10 seconds, and right away when it shows up on the network again. On the Mac and on Windows.
- An AirPlay speaker that loses its connection shows Reconnecting… once its second retry, about 3 seconds in, has failed, and the first retry that works clears it. On the Mac its row and a notice now say so too, as on Windows.
- The web remote now lets in phones on a carrier-grade NAT address, such as the ones Tailscale gives its devices. On the Mac and on Windows.
- On the Mac, while a calibration is running, the calibrate button's hint says another calibration is still running.
- The web remote can turn on an AirPlay speaker that asks for a password once you've typed its password in AudioSync since it started. Before, the phone left it off. On the Mac and on Windows.
- On the Mac, an update's signature is checked before the update is unpacked.

### Fixed

- A speaker whose device takes sound faster than AudioSync can send it, so it plays in bursts while the other speakers play fine, is now restarted once on its own. If that happens again within 30 seconds, it keeps playing and a notice says its link is struggling and to turn the speaker off and on again. On the Mac and on Windows.
- A Bluetooth speaker whose clock runs slow, by up to 1%, no longer drifts later and later until it skips back with a short dropout: AudioSync measures its clock and keeps pace with it. On the Mac and on Windows.
- On Windows, the equalizer picker shows a saved curve by the name you gave it when a preset has the same gains, as on the Mac. Before, a saved all-0 dB curve showed as Flat.
- On Windows, a chosen app that isn't running is listed in name order among the other apps, as on the Mac.
- A Google Cast speaker you added by address keeps the name and settings AudioSync found for it after a relaunch, its zone, presets and on state included, before the network search finds it again. If the search finds it at another address, it shows as one row you can remove. On the Mac and on Windows.
- On the Mac, adding an address whose speaker AudioSync already found at another address leaves that speaker's row as it is.
- When a saved input channel pair the device doesn't have plays as channels 1-2, picking 1-2 no longer restarts the capture, and the pick is kept, so the old pair doesn't come back once the device has those channels again. On the Mac and on Windows.
- On Windows, settings a newer version of AudioSync added are kept when an older version saves. Before, going back a version lost them.
- The PIN prompt of a speaker set to ask for a code every time no longer says it connects without a PIN after pairing. On the Mac and on Windows.
- On Windows, the tray panel says "No connected speakers" when it has nothing to list, as the main window does, instead of staying blank.
- On Windows, with the equalizer off, its gain values and band labels stay readable; only the sliders fade.
- On the Mac, the line under a speaker list's In zones box no longer runs into the box's rounded corners.
- On the Mac, after macOS restarts its audio service AudioSync keeps following each speaker's volume and mute, and the list of outputs.
- On Windows, when the app you play from runs as administrator and quits or restarts, AudioSync now notices within about 2 seconds and stops or switches to the whole system, as your setting says, with the usual notice. Before, streaming went on in silence.
- On Windows, when the PC wakes from sleep while AudioSync streams and Windows audio is slow to come back, AudioSync keeps trying to capture for up to a minute after the wake. Before, it stopped streaming after about 5 seconds of tries. A speaker Windows was refusing to open when the PC went to sleep gets its full 30 seconds again after the wake before AudioSync shows its notice. It hasn't been tried on a real sleep and wake yet.
- On Windows, a speaker that Windows still lists but keeps refusing to open (a Bluetooth speaker that reconnects slowly, say) is no longer switched off after 30 seconds. It stays on with a notice, AudioSync tries it every few seconds, and it plays again by itself once Windows lets it open.
- On the Mac, a speaker whose device macOS still lists but that refuses to start (a Bluetooth speaker that's still reconnecting, a device being set up again after a wake, or a device another app took for itself) is no longer switched off when it starts. It stays on, AudioSync tries it every 3 seconds, and it plays again by itself once the device starts. Any other start error still switches it off with a notice.
- A speaker that drops off for a moment, such as a Bluetooth speaker losing its link, no longer makes your other speakers skip and then go silent while it reconnects. For 10 seconds after it disappears, the others keep their timing, so only that speaker has a gap. If it's still gone after 10 seconds, the others adjust once. On the Mac and on Windows.
- On the Mac, the macOS prompt asking whether AudioSync may control Spotify or Music now stays up until you answer it, for up to 5 minutes. Before, it could keep disappearing and coming back on its own. Until you allow it, AudioSync sends that app no play, pause or skip.
- On the Mac, when the captured audio stops arriving while Spotify or Music still shows it playing, or an input device you stream from stops delivering for 2 seconds, AudioSync opens the capture again while AirPlay and Cast speakers stay connected. Pausing the app, or a quiet system, never counts as the capture stopping.
- On the Mac, from the moment macOS says it's going to sleep until it wakes, AudioSync no longer takes a silent speaker or a quiet capture as stuck, so it doesn't restart either while the Mac goes to sleep. At the wake both checks count from zero again, so a capture that's slow to come back gets its full time before it's opened again.
- A device on the network announcing a flood of fake Google Cast speakers no longer grows the speaker list without end: it keeps up to 128, and the speaker you play to stays listed. If something goes wrong inside AudioSync's Google Cast sender, the speaker stops with an error on its row and can be started again, instead of Google Cast going quiet until AudioSync restarts. On the Mac and on Windows.
- A Google Cast speaker added by address that doesn't answer AudioSync's regular check, for example because it's switched off, is listed as offline instead of online, and if it's on it waits and plays once it answers again. Only a finished secure connection counts as an answer, so a router or filter that takes the connection with no speaker behind it doesn't make it look online. On the Mac and on Windows.
- Removing a Google Cast speaker you added by address now takes it out of its zone too, unless AudioSync also finds it on the network. Before, the zone kept it as a member that read Not found, with no way to drop it. On the Mac and on Windows.
- On Windows, Only This Speaker on a Google Cast speaker that's offline now changes nothing, and a zone click or a preset recall passes over it as busy, as on the Mac. Before, Only This Speaker switched the other speakers off for nothing.
- A local speaker whose sound starts late catches up in one step within its first 3 seconds, instead of playing late or drifting into place. On the Mac and on Windows.
- On the Mac, when AudioSync stalls for a moment and a local speaker comes back late against the others, it jumps back to its delay in one step. Before, it could stay late for the rest of the stream.
- Calibration times a speaker by the sound that reaches the microphone first, so a louder reflection off a wall no longer makes it read late, and a knock or hiss just before the test sound isn't taken for it. On the Mac and on Windows.
- When calibration can't start, its message names only what's off instead of asking for all of it. Calibrate All and Measure Again say it the same way. On the Mac and on Windows.
- With AirPlay 2 on, a paired speaker that also announces it asks for a PIN plays with its stored pairing. Only a speaker that turns the pairing down asks for the PIN again. On the Mac and on Windows.
- With AirPlay 2 on, a speaker that offers AirPlay 1 and AirPlay 2 and turns AirPlay 1 down now gets the AirPlay 2 PIN prompt. On the Mac and on Windows.
- With AirPlay 2 on, two speakers that share one link-local IPv6 address on different network interfaces now each get their own PTP timing. Before, AudioSync took them for one speaker and only one of them got it. On the Mac and on Windows.
- With AirPlay 2 on, a pairing step that never gets an answer no longer leaves the speaker on Connecting. It fails and tries again like any other failed start. On the Mac and on Windows.
- An AirPlay speaker that first shows up with only some of its addresses, say only an IPv6 one it can't be reached on, no longer sits on Connecting while AudioSync waits out each of them. Once the rest of its addresses show up, a start that hasn't connected yet begins again on them. On the Mac and on Windows.
- On the Mac, while macOS asks for microphone access for a new input, picking the source that's already playing cancels the switch.
- On the Mac, the menu bar panel's separator lines and edges stay sharp while it opens. Before, they blurred as the panel grew and snapped sharp at the end. It now unfolds from its arrow at its own size instead of scaling up.
- On the Mac, the menu bar panel's first open after launch grows in from the menu bar like every open after it. Before, that first one just faded in at full size.
- On the Mac, while none of a zone's speakers is found, Tab in the main window skips the zone's greyed round icon and stops on its arrow, which shows or hides the zone's speakers, as in the menu bar panel. Before, the focus stopped on the icon, which VoiceOver doesn't read.
- On the Mac, while an AirPlay problem is showing, the "no speakers found" note asks you to check Local Network access only once macOS has refused it, not while AudioSync merely can't tell yet.
- On the Mac, a saved Switch to All System Audio When the Source App Quits setting that AudioSync doesn't recognize now reads as on, the default, as when nothing was saved. Before, it read as off.
- On the Mac, after the menu bar panel changes size, it moves back under its icon once the icon has moved in from the screen edge that held the panel.
- On the Mac, a damaged value in your saved zones, presets or AirPlay pairings now loses only that value or entry, not all of them. A zone or master volume that can't be read goes back to 100, and a preset whose equalizer gains are missing or damaged is dropped on its own. A damaged pairings file is kept as a copy next to it before AudioSync writes a new one, and one AudioSync can't read at all is left as it is instead of being written over. One damaged entry in the list of permissions AudioSync already explained no longer makes it explain them all again.
- On the Mac, Forget Pairing, which shows while a speaker is paired or AudioSync still keeps its credentials, also deletes the kept-aside copies of the pairings file that still hold that speaker after the main file lost its entry. Before, those copies kept its key.
- A saved Google Cast speakers file that AudioSync can't read is kept as a copy next to it before AudioSync first saves over it, so the delays and formats in it aren't lost. On the Mac and on Windows. On the Mac, a file AudioSync can't open at all is left as it is.
- On the Mac, while the web remote is off, Paired Devices counts the phones it will let back in. Before, one damaged entry in the saved list made it say "No paired devices" while the others still worked.
- A local speaker that goes silent because the system stops taking its sound, while its row still says it plays, now starts again on its own a few seconds after it went quiet. If it keeps going silent, or its device won't start, for example because another app took the device for itself, AudioSync keeps it on and keeps trying. On the Mac, one that then can't start for good, for example because it refuses the format, goes off with a notice instead. On Windows a notice says the speaker was lost and goes once it has played a few seconds. The other speakers keep playing in step. On the Mac and on Windows.
- On Windows, a damaged value in your settings, zones or presets no longer resets all your settings. That one value goes back to its default and the rest is kept. A damaged entry among your AirPlay pairings now costs only that speaker, which pairs again. A damaged entry among the phones paired with the web remote now costs only that phone, which pairs again, on the Mac too. Either way the old file is kept as a copy first.
- On Windows, pasting something that isn't a whole number, such as "1,5", into a delay box no longer leaves just its digits behind ("15"), which Enter then applied. The paste is ignored and the box keeps what it showed, as on the Mac.
- On Windows, the tray panel's Open AudioSync and Exit buttons no longer get cut off at its bottom edge when a notice in it runs over more than one line. On a screen too short for the whole panel, its speaker list gets shorter and scrolls, and when that is not enough, everything between its top row and the buttons scrolls.
- On Windows, when another program resizes the main window past the screen, it now stops at the largest size Windows gives any window, instead of growing taller than the screen.
- On Windows, an AirPlay speaker that was on at launch and asks for a PIN or password goes off with a notice, instead of reading On while nothing plays.
- An AirPlay speaker set to ask for a code every time that you turned on yourself goes off with a notice when streaming stops, and the next click asks for a new code. Before, it stayed on after a stop, and on Windows it then read On while nothing played. On the Mac and on Windows.
- Stopping streaming while an AirPlay speaker that asks for a PIN every time is still pairing, or its PIN prompt is open, no longer turns it off. Type the PIN and it stays on. If the pairing fails before streaming starts again, it turns off with the notice that it needs a PIN, and closing its PIN prompt after the stop cancels your click. On the Mac and on Windows.
- On Windows, after a screen reader sets an AirPlay speaker's volume, the speaker's row follows its own remote again within a few seconds. Before, the row ignored the remote until you dragged its slider or pressed a key on it.
- On Windows, Narrator reads a zone speaker's note, such as "Stayed off, since one Cast speaker plays at a time", from its button in the main window, as it already did under In zones and in the tray panel. Before, it read only "Turn On".
- On Windows, Narrator and other UI Automation tools can now fold and unfold a zone header, in the main window and in the tray panel.
- On Windows, when Windows audio restarts or the captured device drops out for a moment, only the capture starts again. Your speakers keep playing, silent for that moment, and AirPlay and Google Cast speakers stay connected, instead of every speaker stopping and starting again.
- On Windows, a speaker whose format changes while it plays, for example after you change its sample rate in the Sound settings or when a Bluetooth headset switches to its hands-free mode, plays again in the new format within about half a second. Before, it could stay silent for up to a second and a half. The other speakers keep their timing.
- On the Mac, a local speaker whose sample rate or channel count changes while it plays, for example after you pick another rate in Audio MIDI Setup, starts again in the new format right away, with a short gap on that speaker only. Before, it played on through macOS's own conversion, timed by what was measured when it started.
- When another app takes the input device you're streaming for itself (exclusive mode on Windows, hog mode on the Mac), your speakers stay on and AudioSync tries the input again every 3 seconds until that app lets go. Meanwhile the notice says AudioSync couldn't capture audio from the source, and it clears once the input plays again. Before, streaming stopped after about 5 seconds with a notice that the capture was lost. On the Mac and on Windows.
- On Windows, a Bluetooth speaker that starts playing within 3 seconds of connecting restarts once about 2 seconds later, with a short gap, so it doesn't play late for the whole stream. The Mac already does this, and that's where we saw such a speaker stay late before the restart was added. On Windows this hasn't been checked with a real Bluetooth speaker yet.
- On Windows, while you stream the whole system and pause the app that played last, a TV keeps showing its track and cover, as on the Mac, until another app plays, that app stops or quits, or you pick another source. Before, it switched to AudioSync's own title right away. Another app that's also paused doesn't take its place. Play and pause from a speaker's remote go to that same app, and an app that was already paused when you picked it keeps its track there too.
- Turning the equalizer on or off fades over 10 ms instead of clicking, and boosted bands no longer clip: the equalizer turns its level down by as much as the boosts add at their peak. On the Mac and on Windows.
- The web remote shows a paused track as Paused, counts a zone's speakers as on the way their switches show, and says "None of 3 found" for a zone whose speakers are all missing. On the Mac and on Windows.
- On the Mac, the now-playing card's play and pause symbol flips as soon as the player takes the command. Before, it could take up to 2 seconds.
- On the Mac, an AirPlay speaker whose own volume reads below -30 dB no longer starts muted after a reconnect.
- A Google Cast speaker that's on but not playing turns off with the notice that it isn't responding when it leaves the speaker list, so it no longer keeps another Cast speaker from playing. On Windows it also turns off when its session ends after it left. On the Mac and on Windows.
- On the Mac, an AirPlay speaker turned off because it's playing from another device or declined the stream now says so, instead of reading as unreachable.
- On the Mac, two copies of AudioSync opened at the same moment no longer both quit. One stays open.
- On Windows, a settings save that fails is tried again about every 30 seconds and at quit, so a change made just before you sign out isn't lost. Before, a failed save counted as saved.
- On Windows, a new zone or preset never takes the number of a deleted one, so an old preset or a web remote page still open can't reach it. Deleting a zone also takes it out of every preset.
- On Windows, the main window opens on the main screen when the screen it was on is gone, and opening it from the tray leaves it where you put it.
- Quitting while you rename a speaker or zone in place keeps the name you typed. On the Mac and on Windows.
- A Google Cast speaker you added by address keeps its name, zone, presets and Hide Speaker mark once AudioSync finds it on the network. On the Mac it also keeps its auto-connect setting, and one that's playing keeps its loudness. On Windows it also keeps its fixed-volume mark and volume step, and the volume you set for it while it isn't playing. On the Mac and on Windows.
- On the Mac, an AirPlay speaker that answers with an error AudioSync retries stays on while it retries, and one that turns the stream down says it declined the stream.
- On the Mac, after Hide Speaker the keyboard focus lands on a row the window still shows, and the "couldn't start" alert lists speakers the way the rest of the app does.
- On the Mac, VoiceOver announces a new notice once, announces the new delay after a delay step, reads the preset picker as Presets and the current preset, and says whether a delay's controls are open.
- On the Mac, a volume change from a TV's remote during calibration is kept, and an old volume report that arrives late no longer moves the slider.
- On the Mac, the menu bar panel grows as soon as its content does, so its bottom is no longer cut off.
- On the Mac, a zone or preset volume that isn't a valid number no longer keeps your zones and presets from being saved.
- A Google Cast offset is capped at 20 seconds when you set it, the same as when it's read back after a restart. Before, a longer offset played until the next launch and then came back as 20. On the Mac and on Windows.
- A Google Cast speaker group that shows up on the network without an address keeps being tried instead of ending the session, and a group whose leader moved to another speaker counts as back right away. On the Mac and on Windows.
- A Google Cast speaker whose status leaves out its volume or what it's running no longer reads as idle or at full volume; AudioSync keeps what it knew. On the Mac and on Windows.
- When your computer's network address changes and a Google Cast speaker reconnects, it gets the sound from the new address. Before, it was still told to fetch it from the old one. On the Mac and on Windows.
- On Windows, a Google Cast speaker's volume set before it connects, by a preset for example, is kept and set once it connects, and a change made on the speaker itself still shows.
- On Windows, a Google Cast speaker added by a host name that belongs to a speaker AudioSync already found on the network plays as that speaker.
- On Windows, an AirPlay speaker muted for calibration stays muted when its own remote sets a volume while calibration runs.
- The web remote reports its port as in use when another app holds it on IPv4 or IPv6 alone. Before, it started anyway. On the Mac and on Windows.
- On Windows, when network discovery stops, a Google Cast speaker that was on but no longer listed goes off, so another speaker can play. Before, it stayed on with nothing playing.
- On the Mac, a Google Cast speaker that left the list just before discovery stops now gets the search-stopped notice too.
- On the Mac, a saved equalizer preamp or band gain beyond 12 dB either way loads at the nearest limit.
- A Google Cast speaker that reconnects keeps the volume it had, and a volume change made just as it reconnects still reaches it. On the Mac and on Windows.
- On Windows, when another program holds your settings file while AudioSync starts, AudioSync reads it once it's free and uses it, starting the speakers you set to connect automatically. Before, it kept the defaults and could later save them over your settings.
- A Google Cast speaker that rounds its volume to its own steps gets a volume you set only once, and the next volume step goes on from the level the speaker shows. On Windows, the speaker's rounded answer no longer reads as a change made on the speaker itself. On the Mac and on Windows.
- On Windows, if a saved curve is deleted while its Rename dialog is open, the new name no longer goes to another curve. Opening Add by Address again while it's still checking an address drops that check.
- The web remote page notices when its connection has died, for example after the computer slept or the phone woke the page, and connects again. Before, it kept showing what it last had, with no warning. On the Mac and on Windows.
- On Windows, when you stream Several Apps and one of them runs as administrator, AudioSync now stops or switches to the whole system once they have all quit, as your setting says. Before, streaming went on in silence. A mix whose app plays on in a newer copy of itself no longer ends because Windows couldn't list the audio for a moment.
- In the Linux preview, an input with more than two channels offers every channel pair, as on Windows, a device with more than 64 channels plays or captures on its first 64 instead of stopping, and notice times show your local time instead of UTC.
- Switching a zone off also switches off its members that aren't found right now, so a speaker you unplugged stays off when it's back. On the Mac and on Windows.
- On the Mac, the menu bar panel no longer counts AirPlay speakers whose rows are hidden because AirPlay sending failed, and it keeps room for Open and Quit on a small screen while a notice or the permission card shows.
- On the Mac, a speaker you turn off finishes its fade-out even when its format changes during the fade.
- Calibrate All's Apply sets the right AirPlay delay when the AirPlay latency moved or was at its limit, and writes nothing for results measured against a reference speaker that changed or is gone. On the Mac, the run stops if the reference speaker changes. On Windows, Calibrate Zone's Apply no longer goes wrong when an AirPlay speaker in the zone is off. On the Mac and on Windows.
- On Windows, removing the address of a Google Cast speaker that AudioSync found and then lost on the network turns it off. Before, it stayed on with no row, and every other Cast speaker was refused as busy.
- With AirPlay 2 on, stopping a speaker and starting it again right away no longer makes it read Busy. On the Mac and on Windows.
- With AirPlay 2 on, AudioSync no longer keeps a processor core busy while a speaker is slow to answer. On the Mac and on Windows.
- When you change an AirPlay speaker's delay while it's still taking something else, such as new cover art, the new delay starts only once the speaker has been told to drop what it buffered. Before, sound at the new delay could reach it first. On the Mac and on Windows.
- A muted AirPlay 1 speaker that AudioSync mutes by sending it silence no longer plays a moment of sound when it joins a stream that's already playing. On the Mac and on Windows.
- With AirPlay 2 on, when a PIN pairing fails at its last check, AudioSync keeps what the speaker already stored and tries it at the next connect, instead of starting a new PIN pairing. On the Mac and on Windows.
- A Google Cast volume step made before the speaker has reported its volume no longer takes it to near full volume. The step waits until the speaker reports one. On the Mac and on Windows.
- Dragging a Google Cast speaker's volume quickly no longer makes the slider jump back as the speaker's answers come in. On the Mac and on Windows.
- On Windows, when the input device you stream from is unplugged, streaming stays on with a notice that it isn't connected, and the capture starts again once the device is back, on another USB port too. AirPlay Retry keeps waiting for it as well. Before, it gave up after a few tries.
- On Windows, a zone speaker moved to another USB port while AudioSync was closed rejoins its zone.
- On Windows, a zone shows as on exactly when a click on it would switch it off, so a Google Cast member that the click leaves off, because another Cast speaker plays or another app took it over, no longer keeps the zone from showing as on.
- On Windows, Ctrl+N while the Save Curve dialog is open no longer turns it into the New Zone dialog.
- On Windows, while a local speaker restarts after it stopped taking sound or changed format, the other speakers keep their timing instead of shifting until it plays again.
- On Windows, a speaker another app held for itself that then refuses to open, as when Windows rebuilds the device just as that app lets go, is now treated like any speaker Windows refuses, instead of waiting as busy. A speaker lost as the PC goes to sleep is tried again at the wake, not while the PC sleeps.
- On Windows, when the device you capture from disappears just as capture starts, the notice says the device isn't connected or Windows audio isn't running, as it does when it drops out while playing, instead of showing a system error.
- On Windows, when Windows audio stops answering while AudioSync reads a speaker's format, the window no longer freezes. AudioSync gives up on that read after 5 seconds.
- On Windows, an AirPlay speaker that declines the stream and is turned off now says so in its notice, instead of saying AudioSync will try again.
- On Windows, Narrator announces a new notice even while one of the window's own notices is showing, and reads a notice again when it goes away and comes back. The tray icon's tooltip names the newest notice.
- Reloading the web remote page on a phone no longer disconnects that phone's other open remote page. On the Mac and on Windows.
- On Windows, turning the web remote off, changing its port or quitting no longer freezes the window for up to half a second while a phone's page has stopped reading.
- On Windows, Launch at Sign-In shows off when the startup entry points to an AudioSync that was moved or deleted, since that entry starts nothing. Turning the option on points it at this copy.
- On Windows, a stretch of captured audio that Windows marks with a timing error no longer adds a gap of silence.
- In the Linux preview, the tray icon appears when the desktop's tray starts after AudioSync.
- On Windows, after AirPlay sending stops and you click Retry, an AirPlay speaker set to ask for a code every time goes off with the notice that it needs a PIN, and your next click on it asks for a new code. Before, it could show as ready with a code that no longer worked, or stay on with no sound.
- The notice from recalling a preset names a speaker that's gone by the name it last had. On the Mac and on Windows.
- A Google Cast speaker added by its address keeps its own name in a zone and in the notice from recalling a preset after it leaves the network, instead of showing the address. On the Mac and on Windows.
- Recalling a preset leaves on the stereo pair or home theater that plays one of the preset's speakers. On the Mac and on Windows.
- On Windows, when the only Google Cast speaker that's on ends its session on its own without a problem, streaming stops. Before, AudioSync went on capturing with nothing to play to.
- On Windows, when the PC goes to sleep, an AirPlay speaker set to ask for a code every time that you turned on yourself goes off with the notice that it needs a PIN, as when streaming stops. Before, it stayed on through the sleep.
- On the web remote, the error under a speaker, zone or preset goes away once it changes from the computer or another phone. Before, it stayed until you changed something from that phone. On the Mac and on Windows.
- On Windows, a source of several apps is named "Chrome and Spotify" in its notices, as in the source list. Before, the notices read "Chrome, Spotify".
- On Windows, an offset you set on an output that you unplug before AudioSync saves its settings is kept at the next launch. Before, it was lost.
- On Windows, after you forget an AirPlay speaker's pairing, turning it on from the web remote no longer starts a PIN pairing.
- A Google Cast speaker that's listed as online but doesn't answer when you turn it on, for example a TV that was just switched off, turns off after a few tries and tells you it isn't responding. Before, its row said Connecting forever. On the Mac and on Windows.
- On Windows, picking other input channels while the input device is unplugged no longer turns streaming off; the new channels play once the device is back.
- An AirPlay speaker that plays only AirPlay 2 and that you turn on while AirPlay 2 is off now says so, and that AirPlay 2 needs to be on. Before, it said it couldn't play, or on Windows that pairing didn't finish. On the Mac and on Windows.
- An AirPlay speaker that briefly drops off the network while it plays keeps the latency it reported, so its timing against the other speakers stays as it was. Before, AudioSync forgot that latency until the speaker started again. On the Mac and on Windows.
- A Google Cast speaker's Mute button changes to Unmute, and back, as soon as you click it. Before, it waited for the speaker to answer. On the Mac and on Windows.
- On the Mac, VoiceOver says whether streaming is on or off when it reaches the menu bar icon, before the number of notices waiting.
- On the Mac, an AirPlay 2 speaker that has left the network still shows "AirPlay 2" as its kind, and VoiceOver reads that it isn't on the network after its on or off state.
- On the Mac, a calibration whose microphone drops audio while it records now stops and changes nothing, as on Windows. Before, it could set a delay that was off by the audio it lost, without saying so. A microphone that's unplugged during a calibration now stops it at once instead of after a wait.
- On the Mac, Measure Again no longer starts on a speaker whose delay changed after Calibrate All measured it, or after the reference speaker changed, and says why, as on Windows. A calibration whose microphone changed while it ran now says so, instead of saying the speakers changed.

## [0.7.1] - 2026-10-27

### Changed

- In the Mac's menu bar panel and the Windows tray flyout, a zone works like its header in the main window: the round icon switches the zone on or off, and the arrow, the name or the space around them shows or hides the zone's speakers. Before, a click on the name switched the whole zone in the panel and anywhere on the row did in the flyout, and the speakers were always listed. On Windows, showing or hiding a zone's speakers in the main window does the same in the flyout, and the other way round; on the Mac, the main window and the menu bar panel each show or hide them on their own.
- On the Mac, while none of a zone's speakers is found, its greyed round icon shows or hides the zone's speakers, in the main window and the menu bar panel, as on Windows. Before, it did nothing.
- A speaker in a zone keeps a short row in its own section, This Mac (This PC on Windows) or Connected Speakers, under "In zones": its switch, its name and a link to its zone. Its switch there turns only that speaker on or off, and the zone's other speakers stay as they are; Go to Zone and Remove from Zone are in its menu. On the Mac and on Windows.
- Go to Zone puts the keyboard focus on the zone's round icon, or on its arrow while none of the zone's speakers is found, so VoiceOver always reads where the focus lands. On the Mac.
- Only This Speaker, in a speaker's menu under its zone, plays this speaker and turns off the zone's other speakers that are on; speakers you hid stay as they are. When this one is a Google Cast speaker that's offline or that another app took over, nothing switches. Nobody leaves the zone. On the Mac and on Windows.
- On the Mac, double-click a zone's name in the main window to rename it, as you already could with a speaker's name. A single click on the name still shows or hides the zone's speakers.
- On the Mac, double-click the name of a speaker in a zone to rename it in its short row under "In zones", as you already could in its row under the zone.
- On the Mac, a zone's speakers sit in one tinted box in the main window, under the zone and under "IN ZONES", with thin lines between them instead of a box around each speaker that plays. The short rows are a little taller, and the link to the zone after a speaker's name is now a small button with the zone's icon and name. The "IN ZONES" label and the lines between rows start where the speaker names do.
- On the Mac, Switch to All System Audio When the Source App Quits is now on unless you turned it off. When the app you capture quits, AudioSync keeps playing all system audio instead of stopping.
- About AudioSync shows just the version number, on the Mac and on Windows. The Mac no longer adds the build number in parentheses, and Windows no longer lists the audio core's version under it.

### Fixed

- On Windows, while none of a zone's speakers is found, Tab reaches the zone's arrow, in the main window and the tray flyout, and Space or Enter shows or hides its speakers; Go to Zone puts the focus there, and Narrator reads on it that none of them was found, as on the Mac. Before, the keyboard couldn't fold that zone and Go to Zone landed on its … button. A one-speaker zone now reads "1 of 1 speaker on".
- On Windows, a stereo pair or home theater in a zone that can't play while AirPlay 2 is off says under the zone that it needs AirPlay 2, as on the Mac, instead of saying it wasn't found. Every zone row that needs AirPlay 2 is dimmed, as on the Mac.
- On Windows, a Google Cast speaker that is off in a zone says "Stayed off, since one Cast speaker plays at a time" only while another Cast speaker is on, as on the Mac. Before, it said so whenever any other speaker in the zone was on, such as the PC's own speakers.
- On the Mac, a zone click or a preset leaves a Google Cast speaker that another app took over off, as on Windows. Before, AudioSync took it back from that app.
- Opening the Mac's menu bar panel again while it's still fading out no longer leaves it stuck at its old size: it grows and shrinks with its content again.
- The Mac's menu bar panel no longer shows a second, full-size box behind it while it opens or closes; its background and shadow grow with it. Showing or hiding a zone's speakers in the panel resizes it in one motion, and the rows below no longer slide over the ones fading out.

## [0.7.0] - 2026-10-27

### Added

- A Terminal install for the Mac: `curl -fsSL https://raw.githubusercontent.com/yassersutojoyo/audiosync/main/scripts/install.sh | bash` downloads the latest release, checks its SHA-256 checksum and installs or updates AudioSync in Applications, with no Open Anyway step. The DMG stays the main download.
- The Mac disk image opens on a window with an arrow to drag AudioSync to Applications and the Open Anyway steps for the first launch.
- The Mac app updates itself. Check for Updates… in the AudioSync menu looks for a newer release on GitHub, checks the download's signature, installs it and opens AudioSync again; it also checks now and then on its own once you allow it. Builds you make yourself never check.
- Zones, on the Mac and on Windows. They replace speaker groups. A zone is a set of speakers with its own volume, and a speaker is in one zone at a time. Click a zone's round icon to switch its speakers on or off together. A zone turns on at most one Google Cast speaker and never switches off a Cast speaker outside it. Move a speaker with Move to Zone in its menu; a zone you empty stays until you delete it.
- A speaker in a zone plays at its own volume times the zone's volume times the master, on local speakers, AirPlay and Google Cast. A zone volume change reaches its playing speakers right away. When a speaker changes its own volume, say from its buttons, AudioSync moves that speaker's slider and leaves the zone and the master alone. A speaker in no zone plays exactly as before.
- Calibrate Zone… in a zone's menu (the ⋯ button or a right-click on the Mac) runs Calibrate All Speakers over that zone's speakers only, on the Mac and on Windows. A speaker in the zone that's off is listed as Off, isn't measured and is never turned on for it, and speakers outside the zone are muted in what AudioSync sends them for the run, without touching their volume, and keep their delays. A zone with nothing to measure says so instead of starting. When the run is done, the Calibrate Zone button measures the same zone again, and clicking any zone while a calibration runs ends it.
- Speaker presets, on the Mac and on Windows. A preset saves which speakers are on, each one's volume, the zone volumes, the master and the equalizer curve. Recalling it is a full scene: the saved speakers go on, every other speaker except the reference goes off, and the volumes and curve are set. Delays, zones, the preamp and the equalizer switch stay as they are. A saved speaker that can't play, because it isn't found, is hidden, is busy or can't play on its own, is named in one notice. A preset counts as current only while everything still matches it exactly.
- Presets live in a Presets card in the main window, in a row under the source in the menu bar panel or the notification area flyout, and under Window > Presets on the Mac or View > Presets on Windows, where the first nine have Control-Command-1 to 9 (Ctrl+1 to Ctrl+9 on Windows).
- A source can now be several apps at once, on the Mac and on Windows. On the Mac, Several Apps in the source menu plays Only the Checked Apps or All Apps Except the Checked Ones, and it shows a checkmark while such a mix plays. On Windows, Several Apps under the source menu has the same two choices. An app that isn't running is skipped, and AudioSync itself is never part of the mix. On Windows, All Apps Except the Checked Ones leaves out Windows system sounds, such as notification chimes; the Mac takes them in.
- An input device with more than two channels, such as an audio interface, gets an Input Channels menu under the source, on the Mac and on Windows. Pick Channels 1-2, Channels 3-4 and so on, or the last channel alone when the count is odd, which plays on both sides. The choice is kept per device, and a pair the device no longer has plays 1-2. On Windows, Channels 1-2 keeps Windows' own stereo mix of the device, as before.
- An input level meter under the source while a microphone or line in is captured, on the Mac and on Windows. VoiceOver and Narrator read it as Input level, in steps of 10%.
- Web Remote: control your speakers from a phone on the same network. It's off until you turn it on under Web Remote… in the gear menu or the Window menu on the Mac, or in the File menu on Windows. Open the address it shows or scan its QR code, add the page to the Home Screen, open it from there and enter the pairing code, and the phone can switch each speaker on and off, set its volume and delay and Master Volume, and show what's playing. It uses plain HTTP, so keep it to your home network. Forget All Remote Devices unpairs every phone, also while the remote is off.
- Web Remote: the page has an Audio source menu with the app's sources, grouped under Apps and Input devices, several apps included, so the phone can switch the source too, on the Mac and on Windows.
- Web Remote: the page shows your zones and presets. The phone can switch a zone on or off, set its volume and recall a preset. A speaker that asks for a PIN or password stays off with a notice to pair it on the computer first, and the rest plays. While AudioSync calibrates, the page says so and only Unpair works.
- Saved equalizer curves, on the Mac and on Windows. Save the ten bands under a name and the curve shows up in the picker after the presets, up to 50 of them. Rename and Delete act on the saved curve the picker shows, and saving under a name you already used replaces that curve. The preamp is yours: picking a preset or a saved curve leaves it alone.
- Calibrate All Speakers, on the Mac and on Windows. It plays the test sweep on each speaker that's on, one at a time, and lists each speaker's new delay, or why it couldn't be measured. It leaves out headphones and virtual outputs, which the microphone can't hear, and so does Calibrate with Microphone under one speaker; the row says why. Include Anyway on such a row measures it with the others until the results close, for an output that reports the wrong kind. Apply sets them all as one change, Undo puts them all back, and Measure Again repeats one speaker. A Cast speaker that had no offset goes back to none on Undo, so the next run searches for it again, also after a restart.
- The Mac Guide has a Calibration topic: measuring one speaker, Calibrate All Speakers, which outputs are left out and Include Anyway, and what to try when a speaker isn't measured clearly.
- On Windows, Calibrate with Microphone under a local speaker sets its delay for you, as on the Mac. If the microphone is blocked, AudioSync says so and offers to open the privacy settings.
- A speaker group made in the Google Home app now plays as one Cast speaker, with one volume and one offset. Its row names the speakers in the group once the group reports them. Those speakers keep their own rows, but they're busy while the group plays.
- AirPlay 2 has a switch now: turn on "AirPlay 2 (Experimental)" in the app menu or the gear menu on the Mac, or in the File menu on Windows. It's off by default, it's saved, and it takes effect the next time AudioSync starts.
- With AirPlay 2 on, a stereo pair, or an Apple TV with HomePods as its home theater speakers, shows as one row with one switch and one volume. The row says "Stereo pair" or "Home theater group" with the speakers in it, or which one is missing. No real stereo pair or home theater group has been tried yet.
- If a Google Cast speaker doesn't show up, you can add it by its IP address or host name: Add by Address… under Connected Speakers on the Mac, or Speakers > Add by Address… on Windows. AudioSync checks that a speaker answers there and keeps the address for the next launch. The row says "Added by address" until a search finds the speaker, and Remove in its menu forgets it.
- A Google Cast speaker now shows what's playing, with the title, artist, album and cover. When AudioSync doesn't know the track, the speaker gets none.
- Master Volume now scales Google Cast speakers and groups too, on the device's own volume, on the Mac and on Windows.
- AirPlay speakers that show a track's progress now get how far into the track you are and how long it is. On the Mac that comes from Spotify and Music, on Windows from any app that reports its track's length. A track without a known length sends none.
- When you change an AirPlay speaker's volume on the speaker itself, its slider in AudioSync follows, on speakers that report it. With AirPlay 2 on, that includes AirPlay 2 speakers.
- With AirPlay 2 on, a notice tells you when another app holds port 319 or 320, so AirPlay 2 speakers that need PTP timing can't play. It comes back for each speaker that can't play because of it, until the ports are free again. It hasn't been tried yet on a receiver that needs PTP timing.
- Web Remote: too many wrong pairing codes from one device block pairing for that device until the code is renewed, not for every phone, and a paired phone still gets in while other devices hold connections open, also when one device holds them from many addresses.
- The Web Remote window shows the setup as three numbered steps under the QR code, worded for iPhone or Android, and the page shows the same steps until you open it from the Home Screen. On iPhone a pairing made in Safari doesn't carry over to the Home Screen, so the Home Screen comes before the code.
- On the Mac, the AirPlay 2 (Experimental) switch has a note under it in the app menu and the gear menu that says what it does and that it takes effect once you quit and reopen AudioSync.
- Hide Speaker in a speaker's menu takes a speaker you never use out of the main window, the menu bar panel or notification area flyout, Calibrate All and the Web Remote, on the Mac and on Windows. It's turned off first and keeps its volume, delay and name. Hidden Speakers in the gear menu and the Speakers menu on the Mac, or the Speakers menu on Windows, brings it back. The reference output can't be hidden. On Windows, when every connected speaker is hidden, the notification area flyout says so under the PC's own speakers.
- With AirPlay 2 on, AirPlay 2 speakers get the track's title, artist, cover and progress too, on speakers that take them. The cover goes out larger to AirPlay 2 speakers and smaller to AirPlay 1 speakers, and a speaker that turns the large one down gets the small one.
- The buttons on an AirPlay speaker's remote work with AudioSync, on the Mac and on Windows. Play/pause, next and previous go to the app AudioSync shows as playing, and the volume buttons move that speaker's volume in AudioSync.
- AirPlay speakers stop when you pause, on the Mac and on Windows, instead of playing on through the audio they still hold. That goes for a pause from the speaker's remote, from AudioSync's play/pause button or in the player itself, such as Spotify, also under All System Audio. Playing again resumes them, and so does sound from another app a few seconds after the pause. Stopping the player, skipping a track or seeking doesn't count as a pause. It hasn't been tried on a real receiver yet.
- AirPlay speakers start from the latency they report, on the Mac and on Windows, so they line up with each other before you calibrate, and Calibrate All counts it when it listens for an AirPlay speaker. While a speaker is playing, AudioSync doesn't move the shared AirPlay delay under it.
- A zone remembers the name of each of its speakers, so one that isn't around shows under its last name, also after a restart, on the Mac and on Windows.
- Forget Password in a speaker's menu, when AudioSync holds a password for it, on the Mac and on Windows. The speaker asks for the password the next time you turn it on.
- On the Mac, the first time a feature needs a macOS permission (Local Network, audio capture, the microphone or Automation), AudioSync says why before macOS asks, and you can choose Not Now. If you said no, the notice has a button that opens the right pane in System Settings, and the rest of the app keeps working. If you update from an earlier version, Now Playing reads Spotify and Music right away, as before, and macOS asks for Automation itself if it still needs to.
- On the Mac, a Permissions window opens once on a first install and asks for Local Network and audio capture together, so streaming doesn't stop later to ask. Each row shows where that permission stands and links to System Settings if it's refused. The microphone and Automation wait until you use them. Skip for Now closes it, and AudioSync > Set Up Permissions… opens it again.

### Changed

- On the Mac, turning streaming on with every speaker off starts it anyway: the speaker list says no speaker is on, and the first one you turn on starts playing. Before, an alert stopped you. On a first install, This Mac starts on, once. A start AudioSync makes on its own still waits for a speaker.
- Plainer wording across the Mac app, the Windows and Linux app and the web remote: menu items, buttons and labels in Title Case on the desktop apps, notices that say what failed and what to do in fewer sentences, and on the desktop apps a zone with none of its speakers around reads "None of 3 found". On the Mac, Connected Speakers says "Looking for speakers…" while AirPlay takes its first look, as Windows does, and the Guide's headings stand out. A notice that only reports a change shows an info icon instead of a warning.
- Your groups become zones once, at the first launch, with their names and speakers, at full volume. A speaker that was in more than one group stays in the first zone only, and a later group that lost a speaker this way also becomes a preset with all its speakers, their volumes, the master and the curve. On the Mac the old groups are kept, so an older build still has them; an older Windows build can't read the new settings file and starts from defaults.
- A microphone or line in as the source keeps every speaker in step, as any other source does, on the Mac and on Windows. With only local speakers on, they play with little delay. With an AirPlay or Google Cast speaker on too, the local speakers wait for it, so everything plays about 2 seconds after the input. The delays you calibrated with music apply unchanged.
- On the Mac, changing the source or the input channels while streaming keeps the AirPlay and Google Cast speakers connected. Only the local speakers fade out and back in on the new source, so a speaker that asks for a code doesn't ask again. If the new source can't start, the old one plays on under the same speakers and a notice says "Couldn't start" with the new source's name, "still on" with the old one's. While macOS still asks for microphone access to a new input, the old source keeps playing.
- Switching the source or the input channels leaves a much shorter gap on AirPlay speakers, on the Mac and on Windows. AudioSync no longer restarts their stream for it, so on a test receiver the silence went from about 1.7 s to about 0.3 s.
- On the Mac, when the source app quits and Switch to All System Audio When the Source App Quits is on, the AirPlay and Google Cast speakers stay connected while AudioSync moves to all system audio.
- On Windows, a source or input channel change that can't start no longer stops every speaker. The old source plays on under the same speakers, with the same "Couldn't start" notice as on the Mac.
- On Windows, turning streaming on with every speaker off starts it instead of asking for a speaker first. The main window and the notification area flyout say "No speaker is on. The first one you turn on starts playing.", and the first speaker you turn on plays. Once the last speaker goes off again, streaming stops. A start AudioSync makes on its own, such as auto-connect, still waits for a speaker. On a first install the PC's built-in speakers are on from the start; an update keeps your speakers as they were.
- On the Mac, an input device that sends 16-, 24- or 32-bit integer samples can be a source now. Before, only float input was read. AudioSync reads the device as it is and never changes its format.
- On the Mac, a Bluetooth headset's hands-free microphone is no longer offered as a source, as on Windows: recording from it switches the headset to call mode and interrupts its stereo sound. A source saved before says the selected input is a hands-free microphone and asks you to pick another one.
- On the Mac, Calibrate with Microphone and Calibrate All Speakers won't record from a Bluetooth headset's hands-free microphone. If that's the default input, they say so and point to System Settings > Sound.
- The equalizer's preset menu is now a Curve picker, presets first and your saved curves after them, on the Mac and on Windows. When the bands match no curve it reads Custom. On Windows, Custom is no longer an entry in the list, just what the picker shows.
- Equalizer Reset sets the preamp back to 0 dB too, so the picker reads Flat afterwards. The equalizer stays on or off as it was.
- The Equalizer card shows the curve's name while the equalizer is on, instead of just On.
- Undo after Calibrate All Speakers goes away once you change a delay it set, now on the Mac too, so it never overwrites your own change. A new AirPlay latency on its own keeps Undo, on both apps.
- Web Remote: wrong pairing codes sent while no code is valid no longer lock pairing, and a paired phone that hasn't been used for 90 days is forgotten and has to pair again.
- An AirPlay 2 speaker set to ask for a code every time no longer connects by itself, on the Mac and on Windows. Auto-connect leaves it off, and so does the next launch when it was on before; its row says Asks for a code every time. Turning it on yourself still asks for the code. Speakers that ask for a code only once, or for a password, auto-connect as before.
- AirPlay speakers can now wait up to 20 seconds instead of 3, so a long Cast offset or a large Calibrate All result plays in full instead of being cut short.
- An AirPlay speaker's row shows the volume the speaker itself is at, on the Mac and on Windows, so a speaker whose display follows AirPlay's usual volume scale, such as a Samsung TV, shows the same number as AudioSync, whichever side you change it on. While the speaker plays, a change on it shows in its row within about a second. AudioSync still keeps each speaker's own volume under its zone and the master, and a preset saves that own volume as before. The zone and the master scale it but never cap it: a row goes where you move it, even when that takes the own volume past 100, and that volume stays after a restart. Turning the master down and back up brings each speaker back to where it was.
- The arrow keys on a Google Cast speaker's volume slider now move by the step the speaker reports, if it reports one.
- On the Mac, a Bluetooth speaker gets the speaker icon instead of headphones, unless its name says it's headphones, such as AirPods or a name with "headphones", "headset", "earbuds", "earphones" or "buds" in it. Calibration goes by the same names, so a Bluetooth speaker that macOS reports as headphones, such as a soundcore Select 4 Go, is still measured.
- A speaker's name tooltip in the main window starts with its full name, on the Mac and on Windows, so a long name cut short in the row can still be read. On Windows the tooltip wraps instead of cutting the name off, and on the Mac the menu bar panel row has the full name as its tooltip.
- On Windows, the notification area flyout says "Looking for speakers…" while it searches, and "No speakers found" only after 20 seconds without one. The main window's Connected Speakers says "Looking for speakers…" during that search too, with no Google Cast note under it.
- On the Mac, the Add by Address button reads "Checking…" while AudioSync tries the address, and the Web Remote window now says "Control your speakers from a phone on the same network as this Mac."
- On the Mac, the Guide uses the system font and wraps its text to the width of the window.
- On the Mac, a column in the main window fades at its top or bottom only while there's more to scroll that way, and a column that fits no longer bounces.
- On Windows, Use Port in the Web Remote window is only available for a valid port that differs from the current one, or for the same port again after it couldn't be used.
- With AirPlay 2 on, a speaker that asks for a PIN starts playing over the same connection once you enter it. A wrong PIN is asked for again, and after repeated wrong ones the next try waits a little. A speaker that turned its pairing down and is switched on again without a new PIN goes off with a notice.
- On the Mac, with `AUDIOSYNC_DEBUG=1`, the debug log notes each time a local output's audio arrives late, with the output's name.
- With `AUDIOSYNC_DEBUG=1`, the debug log says why a speaker asks for its PIN again, on the Mac and on Windows.
- On the Mac, VoiceOver reads the source menu as Audio Source and the equalizer's menu as Curve, each with what's chosen as its value.
- On the Mac, the notice for AirPlay stopping after an internal error now also says to restart AudioSync if it keeps happening, as on Windows.
- For testing, with `AUDIOSYNC_DEBUG=1`: `AUDIOSYNC_NO_AUTO_CONNECT=1` keeps every speaker off for that launch and saves nothing, `AUDIOSYNC_WEB_REMOTE=1` turns the Web Remote on for one run, the debug log shows each Web Remote pairing code, and on the Mac it says why each session stopped. On the Mac and on Windows.

### Fixed

- A volume step made with an AirPlay TV's own remote stays where the remote put it. Before, AudioSync sent the new level back to the TV, which could round it and undo the step. On the Mac and on Windows.
- On Windows, a phone turned away because the Web Remote is busy now gets the busy message instead of a dropped connection.
- On the Mac, streaming All System Audio while nothing plays no longer brings up the notice that capture permission is missing. The notice now waits for sound.
- On the Mac, calibration works while the source plays nothing. Before, the test sound never started and the run failed as if the source were silent.
- On the Mac, after you type in a delay field, a click on that panel's step button steps from the value you typed, and a click that closes the panel leaves the keyboard focus on the speaker's delay.
- On the Mac and on Windows, a + step on a delay that's already at the 20 s maximum no longer clears the speaker's last calibration result.
- Web Remote: quick taps on the page, such as on the delay - and + buttons, no longer zoom it in on iPhone, and tapping a text field doesn't zoom either. Pinch to zoom still works.
- If a Google Cast speaker's volume turns fixed while you drag its slider, the last drag value no longer gets sent to it.
- With AirPlay 2 on, AudioSync ignores timing packets from devices it isn't playing to, so a flood of them from another computer can't hold up the audio.
- If a Google Cast speaker's Resync fails, AudioSync reconnects and reloads once, and ends the session with the reason if that fails too. Before, the row could stay on Playing over silence.
- A delay set on a Google Cast speaker added by address stays once a search finds that speaker under its own name, on the Mac and on Windows.
- Quitting no longer waits on a Cast device that stopped reading.
- On the Mac, the menu bar panel keeps its text readable over any window behind it, light or dark.
- On the Mac, the menu bar panel now opens as a popover from the icon, with a little arrow pointing at it. It grows out of the icon and shrinks back into it, and a second click on the icon closes it again, like other menu bar items. On recent macOS that second click opened the panel right back up.
- On the Mac, a Bluetooth speaker turned on within 3 seconds of connecting no longer plays late for the whole stream. AudioSync restarts it once, quietly, 2 seconds after it starts.
- On the Mac, the menu bar panel no longer stutters when it opens or closes, also on a quick second open or when you close it while it's still opening. A notice that comes or goes while the panel is open resizes it smoothly, and with Reduce Motion on the panel only fades.
- On the Mac, the Calibrate All list scrolls when you have many speakers, so Apply and Close stay on screen.
- On the Mac, a local speaker that can't start because the source's sample rate is higher than AudioSync can hold now says so, instead of blaming another app or the format.
- On the Mac, a Google Cast offset you set by hand, or with Apply or Undo in Calibrate All, stays put. Before, a late update from the speaker could put the old offset back.
- With AirPlay 2 on, a speaker that answers with an error about its own state, or a server error, keeps its HomeKit pairing and AudioSync tries again later. Only a rejected pairing asks you to pair again.
- On the Mac, Forget All Remote Devices stays available after a save fails, so you can try again.
- On Windows, an auto-connect speaker that can't start no longer brings up a Start notice when no speaker is left on.
- On Windows, Apply and Undo in Calibrate All set the AirPlay delay even while no AirPlay speaker plays, as on the Mac, instead of waiting until one does. Undo puts the old AirPlay delay back even after AirPlay started again with a different latency.
- On the Mac, a paused player keeps its track and cover on AirPlay speakers. Before, a player woken after a long pause could answer with nothing and the speakers flashed AudioSync's own title and icon.
- A source that starts after an AirPlay stream no longer loses its first moment of sound on the AirPlay speakers.
- On the Mac, a mix keeps playing when one of its apps quits while the others still run. Before, it could switch to All System Audio or stop. A source you pick or a stop you make right then is carried out too.
- On the Mac, AudioSync notices within about 6 seconds when a paused source app quits, also when macOS doesn't report the quit.
- On the Mac, with Switch to All System Audio When the Source App Quits on, streaming no longer stops when the app quits right after its sound ended. AudioSync tries all system audio up to 3 times, half a second apart, before it gives up.
- On the Mac, an app in the source that quits without macOS reporting it now leaves the source menu right away, and a quit single source app is noticed about a second sooner.
- An AirPlay speaker stopped while it's still connecting now has its session ended on the speaker too, on AirPlay 1 and AirPlay 2, so turning it on again right away doesn't run into the old session.
- A paired AirPlay speaker that can't prove it's the same speaker no longer plays. AudioSync turns it off with its own notice and keeps the pairing.
- When a speaker turns down its stored pairing, AudioSync keeps it until a new pairing replaces it, on the Mac and on Windows. Cancelling the PIN prompt no longer loses it, and Forget Pairing stays in the speaker's menu.
- A speaker you paired over AirPlay 2 always plays over AirPlay 2 and never falls back to AirPlay 1 on its own. With AirPlay 2 off, its row reads AirPlay 2 only until you forget the pairing.
- An AirPlay 2 speaker or a Google Cast group can no longer make AudioSync hold an endless amount of data. Past a fixed limit, AudioSync closes that connection.
- The connection an AirPlay speaker's remote buttons use closes after a wrong code, and a device that keeps trying is held off for a few seconds.
- On the Mac, the Web Remote's pairing code and a wrong-code lock stay when the Mac's network name changes, instead of starting over.
- An AirPlay 2 speaker set to ask for a code every time that was on when you quit no longer gets paired by auto-connect when it asks again, on the Mac and on Windows. It goes off each time it asks without playing, not only when it first shows up. While its code prompt is open, AudioSync starts no second pairing and plays nothing to it, and on Windows a speaker you turn on while its pairing runs goes off again if the pairing fails or you cancel it.
- On the Mac, under All System Audio, a paused player's track and cover stay on the AirPlay speakers until another player plays, the player stops or it quits.
- On the Mac, picking the source or input channels that are already playing clears the "Couldn't start" notice and changes nothing else.
- In the dark theme the teal text, such as a link or a delay value, is lighter, so it stays readable on a hovered or playing row, and so do the speaker icons in the Mac menu bar panel over a light window. On the Mac and on Windows.
- On Windows, Narrator reads a name for the port box and each button in the Web Remote window.
- After streaming starts with every speaker off, a speaker that goes off before it ever plays, such as one that asks for a code every time, no longer stops streaming: AudioSync waits for the next one. Once a speaker has played, turning the last one off still stops streaming. On the Mac and on Windows.
- An AirPlay speaker that turns the connection down before any pairing starts goes off with a notice that says it didn't accept the connection, instead of one that asks for a PIN or code. On the Mac and on Windows.
- An AirPlay speaker that's retrying when its receiver leaves the network stops retrying along with its row, and its retrying notice goes away. It stays on and joins again once the receiver is back. On the Mac and on Windows.
- In the main window, on the Mac and on Windows, a zone's header works like a speaker row: only its round icon switches the zone on or off, and a click anywhere else on the header, its name, count, arrow and the space around its volume and ⋯ button included, shows or hides the zone's speakers. Before, a click on the name switched the whole zone, and parts of the header did nothing.

### Windows and Linux (preview, not released yet)

- The first release with the Windows 11 version, for x64 and ARM64 PCs, is still to come. It isn't code-signed, so SmartScreen may warn the first time you open it. It has the changes above and in 0.6.0 too, except the entries about the Mac. Google Cast speakers play on Windows as well, "Switch to All System Audio When the Source App Quits" is in the File menu, the Guide's diagrams have descriptions for Narrator, and a click on the empty space of an open delay panel closes it.
- You can rename a speaker: double-click its name, or right-click it or use its … button and choose Rename. Press Enter to keep the new name or Escape to cancel; a click outside the field keeps it too. The name stays after a restart and shows in the notification area flyout, in notices and in the AirPlay code and password dialogs. Leave it empty to go back to the device's own name, which the name's tooltip keeps showing while it's renamed.
- A renamed speaker shows its device's own name under the new one, in the main window and the notification area flyout, and a double-click on the name goes by the double-click speed set in Windows.
- A Now Playing card in the main window follows what Windows reports as playing, with the track, artist and artwork, and Previous Track, play or pause and Next Track buttons.
- View > Appearance shows AudioSync's windows in Light or Dark, or follows Windows with Match System. Ctrl+W closes the window you're in, and Ctrl+Q exits AudioSync from any of its windows.
- The Guide now covers input devices, track info on AirPlay receivers, Launch at Sign-In, AirPlay passwords, and what happens when the source app quits.
- While the Web Remote is on and the PC is connected to a network that Windows marks as Public, the Web Remote window and a notice say anyone on that network can open the pairing page.
- There's a Linux preview too, built from the same code as the Windows app, as an x86_64 AppImage. It captures the whole system or one app through PipeWire, plays to AirPlay 1 and Google Cast speakers (AirPlay 2 behind the same switch) and to your local PipeWire outputs, and shows what's playing from MPRIS players. When PipeWire or WirePlumber restarts, it reconnects and finds your outputs again. Like the Windows build, it hasn't been tried on a real speaker yet.

## [0.6.0] - 2026-10-24

### Added

- Google Cast speakers. They're listed with your other speakers and start and stop with streaming, with their own volume and Mute. One plays at a time, and speaker groups made in the Google Home app can't play. If another app starts casting to a speaker AudioSync plays on, AudioSync turns it off and says so.
- Cast has no shared clock, so AudioSync can't line a Cast speaker up by itself: you set its offset by ear under the speaker, and it's kept for that speaker. If one drifts, Resync under its volume restarts the stream with a short gap. AudioSync never does that on its own.
- When a Cast speaker and AirPlay speakers are on together, the side that's ahead waits for the other. AirPlay can wait at most 3 seconds; past that a notice says how far ahead AirPlay plays and suggests lowering the Cast speaker's offset.
- AirPlay 2, experimental and off unless you start AudioSync with `AUDIOSYNC_AIRPLAY2=1`. It plays one AirPlay 2 speaker at a time: a second one waits with a note under its name and starts once the first stops. A speaker that asks for a PIN gets the pairing prompt, and a pairing the speaker turns down is forgotten, so the next try pairs again. It hasn't been tried on a real AirPlay 2 receiver yet.
- On the Mac, Calibrate with Microphone under a local speaker sets its delay for you. It fades the music out, plays a short, quiet sweep on the built-in speakers and then on that speaker, and sets the delay from the difference. It needs the built-in speakers in the list. If you cancel, the microphone isn't allowed, or the result isn't clear, the delay stays as it was and AudioSync says why.
- The Guide has three diagrams, in light and dark: where the sound goes, what a speaker's delay does, and how a Cast speaker's offset fits with your other speakers. Each has a description for VoiceOver.
- When the app you capture quits, AudioSync can switch to all system audio instead of stopping. Turn on "Switch to All System Audio When the Source App Quits" in the app menu or the gear menu. A notice tells you when it switched.
- On the Mac, you can rename a speaker: double-click its name, or choose "Rename…" from the speaker's … menu. The name stays after a restart and shows in the menu bar panel and in notices too. Clear it to go back to the device's own name, which the speaker's tooltip keeps showing while it's renamed.

### Changed

- A Bluetooth speaker or headset now keeps about 10 ms more audio buffered, so it's less likely to run short right after it starts. When it's the earliest of your local speakers and no AirPlay speaker is on, everything plays up to about 10 ms later against the screen; the delays between your speakers stay as you set them.
- When no AirPlay speaker is on and your earliest local speaker runs at the same sample rate as the captured audio, everything plays about 25 ms later against the screen than before. That comes with the fix below for speakers at the capture rate running short. The delays between your speakers stay as you set them.
- When a local speaker drifts further than AudioSync can smooth out, it now waits longer before a short skip or moment of silence puts it back in time, so those come less often. Until then, such a speaker can sit a little further from the others than before.

### Fixed

- On the Mac, a delay you've typed now applies as soon as you leave its field: when you click into another delay field, move on with Tab, or click any other control, such as a volume slider or the power switch. A step button adds to the value you typed.
- On the Mac, a delay field no longer shows a character it can't take for a moment before removing it: the keystroke is blocked, and the field gets the red outline with a note on what it takes. While the field has the focus, one red outline marks it in place of the blue focus ring.
- On the Mac, a click on the empty space of an open delay panel closes it, like a click anywhere else on the speaker's row.
- On the Mac, a Bluetooth speaker that reports a latency below 50 ms or above 1 second no longer cuts its fade-out short when you switch it off, or holds the stop for the full half second. AudioSync uses the last plausible latency it saw for that speaker instead, or 150 ms. Your delays and the timing between speakers don't change.
- A local speaker that plays later than your earliest one, because you set a delay on it or an AirPlay speaker is on, now keeps its timing when its clock runs a little fast or slow. AudioSync now nudges its speed in small, smoothed steps, where before it drifted until a short skip or moment of silence put it back. Speakers whose clocks keep time are also no longer adjusted back and forth when the system wakes AudioSync a little late now and then.
- A local speaker at the same sample rate as the captured audio no longer runs short for a moment now and then when its clock runs a little fast. It gets the same small speed adjustments as a speaker at another rate, and plays the audio unchanged between them.
- On the Mac, a speaker (often Bluetooth) that started while the system was waking AudioSync late, or whose second wake-up came late, no longer plays a little ahead of your other speakers for the whole session, or skips once right after it starts. Once its wake-ups come on time again, it eases to the timing you set, so the same delay lands on the same timing every session.
- On the Mac, a speaker whose timestamps briefly said it was early no longer ends up that much later than its delay when you change the delay within the next minute, and the debug log no longer counts that minute's wake-ups as late.
- Raising a playing speaker's delay by more than 5 seconds restarts it at the new delay, instead of leaving it silent for the whole increase.

## [0.5.0] - 2026-10-21

### Added

- AirPlay speakers with a screen, such as a TV, show the track that's playing in Spotify or Music: title, artist, album and artwork. When nothing is playing there, or with any other source, they show "AudioSync", the source's name (such as the app or input device) and "From your Mac" instead of a file name. It uses the same Automation access to Spotify or Music as Now Playing in the window.
- When a local speaker you switched on sits at 25% or less, or is muted, in macOS (every output has its own volume there, and only the default one is in the Sound menu), its row says so, and one click sets it to 75% or unmutes it. The menu bar panel shows a small speaker icon on that row.
- A new audio engine written in Rust, which now plays your audio. It replaces the previous engine completely, so the hidden "Use Swift Audio Engine (Next Start)" option from the test builds is gone.

### Changed

- AudioSync is now licensed under the GNU General Public License, version 3 or later (GPL-3.0-or-later), instead of MIT. The About panel shows the copyright and the license, and that it comes with no warranty. Contributions now come with short contributor terms, described in `CONTRIBUTING.md`.
- AirPlay on the Mac now goes through AudioSync's own AirPlay sender, the one the Windows version uses, and the bundled AirPlay engine is gone, so the app is much smaller and runs nothing next to itself. Your AirPlay speakers, their volumes, groups and auto-connect carry over the first time the sender finds them. An Apple TV that asks for a PIN pairs once more, and an AirPlay 1 speaker with a password now works: AudioSync asks for the password when you turn it on. A speaker that only takes AirPlay 2 isn't listed yet, and neither is this Mac's own AirPlay Receiver.
- Delays now work with local speakers alone. The Mac's built-in speakers are the reference and have no delay of their own; every other local speaker's delay is relative to them and always applies, so you can line a Bluetooth speaker up with the built-in speakers (it usually needs about −150 to −300 ms) without any AirPlay speaker on. Lining the Mac up with AirPlay speakers happens in one place now, the AirPlay delay under Connected Speakers. Delays from an earlier version are carried over once, so a setup tuned for AirPlay sounds the same.
- Sound now fades in over about 20 ms when it starts and fades out when it stops, on local and AirPlay speakers alike.
- Building from source now needs a Rust toolchain, since the app links a small Rust library.
- A delay you've typed but not applied yet is marked, with a hint to press Return (Enter on Windows).
- A speaker's delay, shown at the top right of its row, changes as soon as you apply a new one, with a short roll and a brief tint. With Reduce Motion on it fades instead.
- In the main window, the speaker list fades out under the header instead of being cut off by a hard edge, and the header band drags the window again.
- The Guide's setup section is now called First Steps, with clearer tips for lining up your speakers.

### Removed

- The built-in updater, which was never switched on, is gone. Updates stay manual: download the new version from the [Releases](https://github.com/yassersutojoyo/audiosync/releases) page.

### Fixed

- An AirPlay speaker no longer jumps to full volume when streaming starts. Each AirPlay speaker keeps its own volume, and the master slider scales it too, never above its own slider. After the update each AirPlay speaker starts at 60% or lower the first time, so set its slider once.
- After the Mac wakes from sleep, AirPlay speakers that were playing stay on and play again by themselves, instead of being switched off with a "couldn't be reached" notice.
- Ctrl-C in the Terminal you started AudioSync from now quits it cleanly, the same way Quit does, even while an alert is open: the sound fades out and the AirPlay speakers are let go first, and a second Ctrl-C leaves at once. It also ends with the usual exit status (130 for Ctrl-C, 143 when it's told to quit), which only matters if a script started it.
- A PIN or password prompt for an AirPlay speaker now closes by itself when the pairing behind it ends, for example when it times out or fails, instead of staying open with nothing waiting for it. That includes a prompt sitting under another alert: it closes as soon as that alert is gone, without a short flash that could swallow the PIN you typed.
- Switching an AirPlay speaker off and straight back on no longer lets a late answer from the first pairing spoil the second one, which could make it refuse the right PIN.
- An Apple TV that turns down its stored pairing is now paired again instead of being switched off, even when that happens while AudioSync is busy with other speaker updates.
- If the audio engine can't be set up when streaming starts, the message now says exactly that and suggests quitting and reopening AudioSync, instead of pointing at the audio source.
- An AirPlay speaker that sends back broken or odd answers can no longer crash AudioSync. Pairing answers nested too deeply or pointing back at themselves are refused, and so are malformed replies from the speaker. This goes for the Windows version too.
- Building from source also finds Rust when it's installed through Homebrew's rustup.
- A Bluetooth speaker, or any other output that runs at 44.1 kHz, no longer crackles when you switch it on. It used to run short of audio for a moment at the start. This goes for the Windows version too.
- With two Bluetooth speakers on at once, the one whose audio arrives late from macOS no longer runs short of audio and drifts off the delay you set. When an output stops, the debug log now says how many of its audio callbacks came late and by how much at most.
- A 44.1 kHz speaker on Windows or a 96 kHz one on the Mac whose clock runs a touch fast no longer clicks every minute or two. AudioSync now keeps following the speaker's clock and nudges its pace a frame at a time, either way, so one that runs a touch slow no longer jumps ahead every few minutes either. This goes for the Windows version too.
- A delay you type, for a speaker or for AirPlay, now applies once, not at every keystroke, so typing 220 no longer jumps through 2 and 22 ms on the way. Return applies it and keeps the controls open, and Command-S does the same for every delay you typed. A click anywhere on a speaker's row opens or closes its delay controls (the AirPlay note under the last row counts as part of that row), and a click on another speaker's row applies what you typed in the open one first, and that one stays open. A click outside every speaker row applies every delay you typed and closes them all. Escape drops what you typed and closes them, and so does quitting before the delay applies. The delay field takes only digits after an optional + or −, and a number out of range gets a red outline and a line saying the range and what Return applies.
- With several Bluetooth speakers on, one whose audio arrives from macOS a little later minute by minute no longer stutters with short skips and gaps. AudioSync used to take that slow creep for a clock running off and kept correcting it.
- A USB or Bluetooth speaker that came back after being unplugged or disconnected could look like it was playing and still stay silent. It plays again now.
- Speakers now play as far apart as their delays say, to within about 10 ms. When the speaker with the lowest delay was a Bluetooth speaker, or another one running at 44.1 kHz, the others used to play about 20 ms too early next to it, and a speaker set within a few ms of the lowest delay played a little early too. With an AirPlay speaker on, this didn't happen. If you tuned a delay by ear, you may need to nudge it by about 10 to 20 ms. The speaker with the lowest delay now starts a little later than before: a 44.1 kHz Bluetooth speaker on its own plays about 37 ms behind the source instead of about 22. This goes for the Windows version too.

### Windows (preview, not released yet)

- A Windows 11 version, written in Rust, for x64 and ARM64 PCs. It's built to play all system audio or one app on the PC's own outputs and Bluetooth speakers, each with its own delay, through the same equalizer. It isn't released: it goes out with a release only as an unsigned preview, and only after I've tried that build on a real Windows 11 PC.
- AirPlay from Windows, through AudioSync's own AirPlay sender. It plays to receivers that take AirPlay 1, and the PC's own outputs wait for the AirPlay speakers so everything stays in step. An Apple TV that asks for a code gets a PIN prompt, and the pairing is kept for the current Windows user until Forget Pairing. A receiver that asks for a password opens a prompt when you turn it on, and AudioSync keeps the password in memory until it quits. Receivers that only play AirPlay 2 are listed with a note and stay off. This part hasn't been tried with a real speaker or Apple TV yet.
- A Windows 11 look: Mica in the title bars, the system's rounded corners and shadow on the tray flyout, and speaker rows that line up.
- When the source can also be heard straight from an output you listen to, a notice says you may hear it twice, and Show How opens a short guide with the steps and a button for the right Settings page.
- Bluetooth hands-free endpoints (the call profile) are left out of the speaker list, since they carry mono call audio and opening one puts the headset in call mode. A headset's stereo output is still listed.
- The Windows version remembers your choices between launches: the source, which speakers are on with their delays and volumes, the master volume, auto-connect, groups, the equalizer, Show Virtual Devices and the window's size, place and whether it was maximized. Like the Mac, it doesn't start streaming by itself at launch unless an auto-connect speaker is there, and a speaker that's unplugged keeps its settings for when it's back.
- The AirPlay delay field takes what you typed when you press Enter or click an empty spot in the window, and Escape puts back the delay that's in use. Before, with a receiver that has a high latency, it could rewrite the field right after the first digit.
- A speaker's delay field works the same way now: Enter applies what you typed and keeps its sync controls open, Ctrl+S (File > Apply Typed Delay) does the same for every delay you typed, a click anywhere on the speaker's row or on the AirPlay note under the last row opens or closes them, and a click outside every speaker row applies every delay you typed and closes them all. The field takes only digits after an optional + or −, and a number out of range gets a red outline and a line saying the range and what Enter applies. Escape puts back the delays in use and closes them, and quitting drops what you typed. Before, typing 220 re-timed the speaker at 2 and 22 ms on the way.
- When the only outputs are virtual ones and they're hidden, Connected Speakers says so and points you to View > Show Virtual Devices.
- A virtual output you turn off stays in the list for the rest of the session, instead of disappearing from under the keyboard focus.
- If you switch on a second Apple TV while another one is showing its PIN prompt, it says "Waiting to pair…" and gets its own prompt next.
- The same pairing fix as on the Mac: switching an AirPlay speaker off and straight back on no longer lets the first pairing's late answer spoil the second one.
- Opening AudioSync a second time brings the running window forward instead of starting a second copy. When the first copy runs as administrator, a normal launch doesn't start a second copy either, but it can't bring that window forward. If you open it while the running copy is still quitting, the new one starts once the old one is gone, as long as that takes no more than a few seconds.
- The pairing wording matches the Mac: the dialog is called "Pair with" and the speaker's name, it asks for a PIN, and it says that Cancel turns the speaker off and that it connects without a PIN once paired. A few notices read more clearly too.
- A debug log for testing: start it with `AUDIOSYNC_DEBUG=1` and it writes `%TEMP%\audiosync-debug.log` (see [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md)).
- An output that another app holds for itself (exclusive mode) stays on instead of being switched off. AudioSync tries it again every few seconds and plays to it as soon as it's free, and a notice says what to do: close that app, or stop apps from taking exclusive control of it in Sound settings.
- When Windows restarts its audio service, the capture starts again by itself. It gets a few tries over about five seconds before streaming stops.
- If AirPlay can't start at launch, or can't start looking for speakers, you now get the notice "AirPlay stopped or couldn't start" with Retry, the same one as when it stops later.
- A speaker that's on but can't play right now says so: "Waiting for device" while another app holds it, "Reconnecting…" while an AirPlay speaker comes back.
- When no AirPlay speaker turns up after about 20 seconds, the list says to check that the PC is on the same network, that Windows Firewall allows AudioSync, or to set the network to Private.
- Keyboard fixes: Escape closes the PIN, group name and About dialogs wherever the focus is, the focus stays on the same control or moves to a neighbouring row when the list changes instead of getting lost, and Narrator says which notice a dismiss button closes and what happened, such as "Kitchen didn't start, turned off" or "settings unreadable, copy kept".
- Settings or Apple TV pairings that can't be read at launch are no longer overwritten. The old file is kept as a copy in AudioSync's data folder and a notice tells you where. You also get a notice when your settings or pairings can't be saved, and when Windows has no working sound output.
- The window fits a 1366×768 screen at 150% scaling.
- As on the Mac, when a speaker that's on sits at 25% or less, or is muted, in Windows, its row says so and one click sets it to 75% or unmutes it. If Windows audio stops answering, reading or fixing that volume can no longer freeze the window: AudioSync gives up after five seconds and tries again a little later.
- A speaker that disappears always gets its notice, a USB device plugged into another port keeps its settings, short Windows audio errors (after sleep, for example) are retried, and an output that doesn't answer holds up the window for about five seconds at most.
- Launch at Sign-In, in the File menu and the tray menu, starts AudioSync in the tray when you sign in to Windows. If Windows has it turned off under Settings > Apps > Startup, AudioSync opens that page and a notice says to turn it on there.
- An input device, such as a microphone or line in, can be the source. A notice tells you when it isn't connected, gets unplugged while streaming, or is blocked by Windows' microphone privacy settings, with a button that opens them.
- AirPlay receivers with a screen, such as a TV, show the playing track's title, artist, album and artwork, read from the media sessions Windows lists. Without a track they show AudioSync's own labels with the source's name, and an input device's name as the artist, as on the Mac.
- Help > AudioSync Guide (F1) opens a guide written for Windows. It doesn't cover input devices, track info, Launch at Sign-In or AirPlay passwords yet.
- The PIN dialog no longer cuts off its contents, the main window can't be made smaller than its controls need, and a speaker's volume slider has its own line in the speaker's row.
- Stopping a speaker lets its short fade-out play to the end instead of cutting off the last few milliseconds.

## 0.4.1 - 2026-10-17

### Changed

- The prebuilt app's AirPlay engine now works on macOS 15 (Sequoia) and later, whichever macOS the release was built on. You no longer need to build from source on macOS 15.
- The bundled engine is smaller (about 30 MB instead of 46 MB): its libraries are built from pinned sources, and FFmpeg no longer includes video encoders AudioSync never used. The engine as distributed is now GPL-2.0-or-later; see `THIRD_PARTY_NOTICES.md`.

### Fixed

- The AirPlay engine in the 0.4.0 download could crash on macOS 26.0 through 26.3, because one of its bundled libraries needed macOS 26.4.

## 0.4.0 - 2026-10-17

The big one: AudioSync is now a regular Dock app with a main window, and it can pair with speakers that ask for a code.

### Added

- A main window (source, Now Playing, master volume, equalizer, groups and speakers) and app menus. The menu bar item now opens a compact panel instead of the old popover. Closing the window keeps AudioSync running in the menu bar; the Dock icon brings it back.
- PIN and password pairing for AirPlay speakers that ask for one, such as an Apple TV. AudioSync only asks when you turn the speaker on yourself; one turned on by auto-connect or at launch is switched off with a notice instead. Speakers show "Asks for a code" or "Password not supported" when they can't simply play.
- AirPlay engine status in the speaker list: "Starting AirPlay…" while it starts, and the reason plus a Retry button when it can't start or stopped after repeated crashes. If the engine needs a newer macOS, the list says so and speakers on this Mac keep working.
- Several notices can wait at once, each clears itself when its cause is gone, and a dot next to the menu bar icon shows one is waiting.
- Groups can be renamed (blank names are rejected).
- Hints when speakers are missing (Local Network access) or when Automation access to Spotify or Music is denied.
- Show Virtual Devices in the gear menu.
- New logo, app icon and menu bar icon.

### Changed

- Speakers are listed under This Mac, then groups, then Connected Speakers: AirPlay and every other output in one list by name, each with a small icon for its kind that turns to color while it plays. The menu bar panel uses the same order.
- Local speakers only apply their sync delay while an AirPlay speaker is on; on their own they play right away.
- Virtual outputs (loopback drivers, meeting apps, aggregate and multi-output devices) are hidden unless you already use one.
- Switching streaming off keeps your speakers chosen; they show as "will play" and come back when streaming is switched on.
- Sync delays are shown and typed in milliseconds.
- Accessibility: the equalizer works with the keyboard and VoiceOver and shows correct half-dB labels, every image-only control has a label, and small text is easier to read.
- The AudioSync Guide covers the main window, groups, pairing, engine status and notices.
- The bundled engine carries eight small patches (up from five), adding password pairing through its API, removal of speakers that stop advertising, and a fresh pairing when a speaker rejects its saved key. See `THIRD_PARTY_NOTICES.md`.

### Fixed

- Starting streaming before the source plays no longer raises a false "Audio is not being captured" alert. AudioSync shows it's waiting for audio and only blames the permission when Now Playing shows the source playing.
- A speaker that rejects its saved pairing key pairs again (HomePods without a prompt, speakers that show a code with one) instead of looking unreachable.
- This Mac's own AirPlay receiver no longer shows up as a speaker (selecting it looped the audio back into the Mac).
- AirPlay speakers that leave the network are removed from the list, and a speaker renamed while AudioSync runs keeps working.
- Quitting can't hang: pending speaker changes get a short time to reach the engine before it stops.
- Automatic source stops no longer show a modal alert.

### Security

- The engine's per-run password is kept off disk once the engine has loaded it, and its config file is readable only by your user.

### Release tooling

- From 0.4.0 on, every release attaches the exact source of the bundled GPL and LGPL libraries (collected by the new `scripts/archive_sources.sh`) next to the patched engine source. The written offer in `THIRD_PARTY_NOTICES.md` stays as a fallback. `RELEASE.md` now matches how releases are actually built (locally signed, not notarized).

### Known limitations

- Older AirPlay 1 receivers protected by a password can't be used; turn the password off on the receiver.
- The prebuilt app's AirPlay engine needs macOS 26 (Tahoe) or later. On macOS 15, build from source.

## 0.3.0 - 2026-10-16

The first public release, an early alpha.

### Added

- Play all system audio, a single app (with its helper processes) or an input device on AirPlay and AirPlay 2 speakers and every local output (built-in, headphones, HDMI/DisplayPort, USB and Bluetooth) at the same time.
- A per-speaker sync delay of 0–20 s, with clock drift correction.
- Per-speaker and master volume, and a 10-band equalizer with preamp and presets.
- Now Playing for Spotify and Music, speaker groups, per-speaker auto-connect and launch at login.
- A bundled open-source AirPlay engine (GPL-2.0-or-later, with five small patches), managed by AudioSync. Its source is attached to the release.

### Known limitations

- About two seconds of latency, because of the AirPlay buffer.
- AirPlay 2 support is reverse-engineered, so a HomePod or macOS update can break it.
- Now Playing works with Spotify and Music only.
- The prebuilt app needs macOS 26 (Tahoe) or later.

[Unreleased]: https://github.com/yassersutojoyo/audiosync/compare/v0.7.1...HEAD
[0.7.1]: https://github.com/yassersutojoyo/audiosync/releases/tag/v0.7.1
[0.7.0]: https://github.com/yassersutojoyo/audiosync/releases/tag/v0.7.0
[0.6.0]: https://github.com/yassersutojoyo/audiosync/releases/tag/v0.6.0
[0.5.0]: https://github.com/yassersutojoyo/audiosync/releases/tag/v0.5.0
