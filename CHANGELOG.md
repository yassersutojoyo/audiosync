# Changelog

User-visible changes in each version. Loosely follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and [Semantic Versioning](https://semver.org/). Downloads are on the [Releases](https://github.com/yassersutojoyo/audiosync/releases) page.

## [Unreleased]

### Added

- On the Mac, you can rename a speaker: double-click its name, or choose "Rename…" from the speaker's … menu. The name stays after a restart and shows in the menu bar panel and in notices too. Clear it to go back to the device's own name, which the speaker's tooltip keeps showing while it's renamed.
- On Windows too, you can rename a speaker: double-click its name, or right-click it or use its … button and choose Rename. Press Enter to keep the new name or Escape to cancel; a click outside the field keeps it too. The name stays after a restart and shows in the notification area flyout, in notices and in the AirPlay code and password dialogs. Leave it empty to go back to the device's own name, which the name's tooltip keeps showing while it's renamed.

### Changed

- A Bluetooth speaker or headset now keeps about 10 ms more audio buffered, so it's less likely to run short right after it starts. When it's the earliest of your local speakers and no AirPlay speaker is on, everything plays up to about 10 ms later against the screen; the delays between your speakers stay as you set them.

### Fixed

- On the Mac, a delay you've typed now applies as soon as you leave its field: when you click into another delay field, move on with Tab, or click any other control, such as a volume slider or the power switch. A step button adds to the value you typed.
- On the Mac, a delay field no longer shows a character it can't take for a moment before removing it: the keystroke is blocked, and the field gets the red outline with a note on what it takes. While the field has the focus, the red outline replaces the blue focus ring instead of clashing with it.
- On the Mac and on Windows, a click on the empty space of an open delay panel closes it, like a click anywhere else on the speaker's row.
- On the Mac, a Bluetooth speaker that reports a latency below 50 ms or above 1 second no longer cuts its fade-out short when you switch it off, or holds the stop for the full half second. AudioSync uses the last plausible latency it saw for that speaker instead, or 150 ms, and the debug log notes it. Your delays and the timing between speakers don't change.

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

[Unreleased]: https://github.com/yassersutojoyo/audiosync/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/yassersutojoyo/audiosync/releases/tag/v0.5.0
