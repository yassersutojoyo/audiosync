# Changelog

User-visible changes in each version. Loosely follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and [Semantic Versioning](https://semver.org/). Downloads are on the [Releases](https://github.com/yassersutojoyo/audiosync/releases) page.

## [0.8.0] - unreleased

### Added

- Zones, on the Mac and on Windows. They replace speaker groups. A zone is a set of speakers with its own volume, and a speaker is in one zone at a time. Click a zone's name to switch its speakers on or off together. A zone turns on at most one Google Cast speaker and never switches off a Cast speaker outside it. Move a speaker with Move to Zone in its menu; a zone you empty stays until you delete it.
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
- With AirPlay 2 on, a stereo pair, or an Apple TV with HomePods as its home theater speakers, shows as one row with one switch and one volume. The row says "Stereo pair" or "Home theater group" with the speakers in it, or which one is missing. Like the rest of AirPlay 2, this hasn't been tried on real speakers yet.
- If a Google Cast speaker doesn't show up, you can add it by its IP address or host name: Add by Address… under Connected Speakers on the Mac, or Speakers > Add by Address… on Windows. AudioSync checks that a speaker answers there and keeps the address for the next launch. The row says "Added by address" until a search finds the speaker, and Remove in its menu forgets it.
- A Google Cast speaker now shows what's playing, with the title, artist, album and cover. When AudioSync doesn't know the track, the speaker gets none.
- Master Volume now scales Google Cast speakers and groups too, on the device's own volume, on the Mac and on Windows.
- AirPlay speakers that show a track's progress now get how far into the track you are and how long it is. On the Mac that comes from Spotify and Music, on Windows from any app that reports its track's length. A track without a known length sends none.
- When you change an AirPlay speaker's volume on the speaker itself, its slider in AudioSync follows, on speakers that report it. With AirPlay 2 on, that includes AirPlay 2 speakers.
- With AirPlay 2 on, a notice tells you when another app holds port 319 or 320, so AirPlay 2 speakers that need PTP timing can't play. It comes back for each speaker that can't play because of it, until the ports are free again. It hasn't been tried on a real receiver yet.
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

### Changed

- Plainer wording across the Mac app, the Windows and Linux app and the web remote: menu items, buttons and labels in Title Case on the desktop apps, notices that say what failed and what to do in fewer sentences, and on the desktop apps a zone with none of its speakers around reads "None of 3 found". On the Mac, Connected Speakers says "Looking for speakers…" while AirPlay takes its first look, as Windows does, and the Guide's headings stand out. A notice that only reports a change shows an info icon instead of a warning.
- Your groups become zones once, at the first launch, with their names and speakers, at full volume. A speaker that was in more than one group stays in the first zone only, and a later group that lost a speaker this way also becomes a preset with all its speakers, their volumes, the master and the curve. On the Mac the old groups are kept, so an older build still has them; an older Windows build can't read the new settings file and starts from defaults.
- A microphone or line in as the source keeps every speaker in step, as any other source does, on the Mac and on Windows. With only local speakers on, they play with little delay. With an AirPlay or Google Cast speaker on too, the local speakers wait for it, so everything plays about 2 seconds after the input. The delays you calibrated with music apply unchanged.
- On the Mac, changing the source or the input channels while streaming keeps the AirPlay and Google Cast speakers connected. Only the local speakers fade out and back in on the new source, so a speaker that asks for a code doesn't ask again. If the new source can't start, the old one plays on under the same speakers and a notice says "Couldn't start" with the new source's name, "still on" with the old one's. While macOS still asks for microphone access to a new input, the old source keeps playing.
- On the Mac, when the source app quits and Switch to All System Audio When the Source App Quits is on, the AirPlay and Google Cast speakers stay connected while AudioSync moves to all system audio.
- On Windows, a source or input channel change that can't start no longer stops every speaker. The old source plays on under the same speakers, with the same "Couldn't start" notice as on the Mac.
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
- An AirPlay speaker's row shows the volume the speaker itself is at, on the Mac and on Windows, so a speaker whose display follows AirPlay's usual volume scale, such as a Samsung TV, shows the same number as AudioSync, whichever side you change it on. While the speaker plays, a change on it shows in its row within about a second. AudioSync still keeps each speaker's own volume under its zone and the master, and a preset saves that own volume as before. Moving a row past what the zone and the master allow stops at the loudest they allow.
- The arrow keys on a Google Cast speaker's volume slider now move by the step the speaker reports, if it reports one.
- On the Mac, a Bluetooth speaker gets the speaker icon instead of headphones, unless its name says it's headphones, such as AirPods or a name with "headphones", "headset", "earbuds", "earphones" or "buds" in it. Calibration goes by the same names, so a Bluetooth speaker that macOS reports as headphones, such as a soundcore Select 4 Go, is still measured.
- A speaker's name tooltip in the main window starts with its full name, on the Mac and on Windows, so a long name cut short in the row can still be read. On Windows the tooltip wraps instead of cutting the name off, and on the Mac the menu bar panel row has the full name as its tooltip.
- On Windows, the notification area flyout says "Looking for speakers…" while it searches, and "No speakers found" only after 20 seconds without one.
- On the Mac, the Add by Address button reads "Checking…" while AudioSync tries the address, and the Web Remote window now says "Control your speakers from a phone on the same network as this Mac."
- On the Mac, the Guide uses the system font and wraps its text to the width of the window.
- On the Mac, a column in the main window fades at its top or bottom only while there's more to scroll that way, and a column that fits no longer bounces.
- On Windows, Use Port in the Web Remote window is only available for a valid port that differs from the current one, or for the same port again after it couldn't be used.
- With AirPlay 2 on, a speaker that asks for a PIN starts playing over the same connection once you enter it. A wrong PIN is asked for again, and after repeated wrong ones the next try waits a little. A speaker that turned its pairing down and is switched on again without a new PIN goes off with a notice.
- On the Mac, with `AUDIOSYNC_DEBUG=1`, the debug log notes each time a local output's audio arrives late, with the output's name.
- With `AUDIOSYNC_DEBUG=1`, the debug log says why a speaker asks for its PIN again, on the Mac and on Windows.
- On the Mac, VoiceOver reads the source menu as Audio Source and the equalizer's menu as Curve, each with what's chosen as its value.
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
- On the Mac and on Windows, a click on the empty part of a zone's header, past its name, shows or hides the zone's speakers like the arrow does. Before, it switched the whole zone on or off.

### Windows and Linux (preview, not released yet)

- The first release with the Windows 11 version, for x64 and ARM64 PCs, is still to come. It isn't code-signed, so SmartScreen may warn the first time you open it. It has the changes above and in 0.6.0 too, except where an entry says "On the Mac". Google Cast speakers play on Windows as well, "Switch to All System Audio When the Source App Quits" is in the File menu, the Guide's diagrams have descriptions for Narrator, and a click on the empty space of an open delay panel closes it.
- You can rename a speaker: double-click its name, or right-click it or use its … button and choose Rename. Press Enter to keep the new name or Escape to cancel; a click outside the field keeps it too. The name stays after a restart and shows in the notification area flyout, in notices and in the AirPlay code and password dialogs. Leave it empty to go back to the device's own name, which the name's tooltip keeps showing while it's renamed.
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

[0.8.0]: https://github.com/yassersutojoyo/audiosync/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/yassersutojoyo/audiosync/releases/tag/v0.6.0
[0.5.0]: https://github.com/yassersutojoyo/audiosync/releases/tag/v0.5.0
