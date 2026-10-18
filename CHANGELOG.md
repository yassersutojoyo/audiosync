# Changelog

User-visible changes in each version. Loosely follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and [Semantic Versioning](https://semver.org/). Downloads are on the [Releases](https://github.com/yassersutojoyo/audiosync/releases) page.

## [Unreleased]

### Added

- AirPlay speakers with a screen, such as a TV, show the track that's playing in Spotify or Music: title, artist, album and artwork. With any other source they show "AudioSync" instead of the pipe's file name.

### Fixed

- Switching streaming off and on while music plays no longer leaves the AirPlay speakers a second or two behind your other speakers for the rest of the session. They also keep their timing when the audio stops for a moment with streaming on, for example across sleep. The bundled engine gets a ninth small patch for this; see `THIRD_PARTY_NOTICES.md`.

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
