# Changelog

User-visible changes in each version. Loosely follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and [Semantic Versioning](https://semver.org/). Downloads are on the [Releases](https://github.com/yassersutojoyo/audiosync/releases) page.

## [Unreleased]

### Added

- AirPlay speakers with a screen, such as a TV, show the track that's playing in Spotify or Music: title, artist, album and artwork. When nothing is playing there, or with any other source, they show "AudioSync", the source's name (such as the app or input device) and "From your Mac" instead of a file name. It uses the same Automation access to Spotify or Music as Now Playing in the window.
- When a local speaker you switched on sits at 25% or less, or is muted, in macOS (every output has its own volume there, and only the default one is in the Sound menu), its row says so, and one click sets it to 75% or unmutes it. The menu bar panel shows a small speaker icon on that row.
- A new audio engine written in Rust, which now plays your audio. It replaces the previous engine completely, so the hidden "Use Swift Audio Engine (Next Start)" option from the test builds is gone.

### Changed

- AirPlay on the Mac now goes through AudioSync's own AirPlay sender, the one the Windows version uses, and the bundled AirPlay engine is gone, so the app is much smaller and runs nothing next to itself. Your AirPlay speakers, their volumes, groups and auto-connect carry over the first time the sender finds them. An Apple TV that asks for a PIN pairs once more, and an AirPlay 1 speaker with a password now works: AudioSync asks for the password when you turn it on. A speaker that only takes AirPlay 2 isn't listed yet, and neither is this Mac's own AirPlay Receiver.
- Delays now work with local speakers alone. The Mac's built-in speakers are the reference and have no delay of their own; every other local speaker's delay is relative to them and always applies, so you can line a Bluetooth speaker up with the built-in speakers (it usually needs about −150 to −300 ms) without any AirPlay speaker on. Lining the Mac up with AirPlay speakers happens in one place now, the AirPlay delay under Connected Speakers. Delays from an earlier version are carried over once, so a setup tuned for AirPlay sounds the same.
- Building from source now needs a Rust toolchain, since the app links a small Rust library.
- The Guide's setup section is now called First Steps, with clearer tips for lining up your speakers.

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
- A delay you type, for a speaker or for AirPlay, now applies when you press Return or leave the field, not at every keystroke, so typing 220 no longer jumps through 2 and 22 ms on the way. Escape drops what you typed, and a value you typed is still kept if you quit before pressing Return.

### Windows (preview, not released yet)

- A Windows 11 version, written in Rust, for x64 and ARM64 PCs. It's built to play all system audio or one app on the PC's own outputs and Bluetooth speakers, each with its own delay, through the same equalizer. It isn't released: it goes out with a release only as an unsigned preview, and only after I've tried that build on a real Windows 11 PC.
- AirPlay from Windows, through AudioSync's own AirPlay sender. It plays to receivers that take AirPlay 1, and the PC's own outputs wait for the AirPlay speakers so everything stays in step. An Apple TV that asks for a code gets a PIN prompt, and the pairing is kept for the current Windows user until Forget Pairing. Receivers that ask for a password, or that only play AirPlay 2, are listed with a note and stay off. This part hasn't been tried with a real speaker or Apple TV yet.
- A Windows 11 look: Mica in the title bars, the system's rounded corners and shadow on the tray flyout, and speaker rows that line up.
- When the source can also be heard straight from an output you listen to, a notice says you may hear it twice, and Show How opens a short guide with the steps and a button for the right Settings page.
- Bluetooth hands-free endpoints (the call profile) are left out of the speaker list, since they carry mono call audio and opening one puts the headset in call mode. A headset's stereo output is still listed.
- The Windows version remembers your choices between launches: the source, which speakers are on with their delays and volumes, the master volume, auto-connect, groups, the equalizer, Show Virtual Devices and the window's size, place and whether it was maximized. Like the Mac, it doesn't start streaming by itself at launch unless an auto-connect speaker is there, and a speaker that's unplugged keeps its settings for when it's back.
- The AirPlay delay field takes what you typed when you press Enter or leave the field, and Escape puts back the delay that's in use. Before, with a receiver that has a high latency, it could rewrite the field right after the first digit.
- A speaker's delay field works the same way now: what you typed applies on Enter, when you leave the field or when you close that speaker's sync controls, and Escape puts back the delay in use. Before, typing 220 re-timed the speaker at 2 and 22 ms on the way.
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

## [0.4.1] — 2026-10-17

### Changed

- The prebuilt app's AirPlay engine now works on macOS 15 (Sequoia) and later, whichever macOS the release was built on. You no longer need to build from source on macOS 15.
- The bundled engine is smaller (about 30 MB instead of 46 MB): its libraries are built from pinned sources, and FFmpeg no longer includes video encoders AudioSync never used. The engine as distributed is now GPL-2.0-or-later; see `THIRD_PARTY_NOTICES.md`.

### Fixed

- The AirPlay engine in the 0.4.0 download could crash on macOS 26.0 through 26.3, because one of its bundled libraries needed macOS 26.4.

## [0.4.0] — 2026-10-17

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

## 0.3.0 — 2026-10-16

The first public release, an early alpha.

### Added

- Play all system audio, a single app (with its helper processes) or an input device on AirPlay and AirPlay 2 speakers and every local output (built-in, headphones, HDMI/DisplayPort, USB and Bluetooth) at the same time.
- A per-speaker sync delay of 0–20 s, with clock drift correction.
- Per-speaker and master volume, and a 10-band equalizer with preamp and presets.
- Now Playing for Spotify and Music, speaker groups, per-speaker auto-connect and launch at login.
- The AirPlay engine ([OwnTone](https://github.com/owntone/owntone-server) 29.2 with five small patches), bundled and managed by AudioSync. Its source is attached to the release.

### Known limitations

- About two seconds of latency, because of the AirPlay buffer.
- AirPlay 2 support is reverse-engineered, so a HomePod or macOS update can break it.
- Now Playing works with Spotify and Music only.
- The prebuilt app needs macOS 26 (Tahoe) or later.

[Unreleased]: https://github.com/yassersutojoyo/audiosync/compare/v0.4.1...HEAD
[0.4.1]: https://github.com/yassersutojoyo/audiosync/compare/v0.4...v0.4.1
[0.4.0]: https://github.com/yassersutojoyo/audiosync/releases/tag/v0.4
