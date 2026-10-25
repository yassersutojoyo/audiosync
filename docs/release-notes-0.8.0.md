<!-- Draft for the 0.8.0 release. RELEASE.md keeps release notes out of the repository, so this file comes out before anything merges. -->
<!-- Placeholder: security fixes not on main yet, to add once merged: the pairing-trust fixes (stored HomeKit credentials kept on rejection, no fall back to AirPlay 1 while pinned), the bounded queues, and the silent session stop. -->
<!-- Placeholder: whether the Windows preview and the Linux AppImage ride along is decided at release time, per RELEASE.md. -->

This one skips 0.7.0, which never shipped, so it carries that work too. The big ones: zones and presets in place of speaker groups, a web remote for your phone, Calibrate All Speakers, and Google Cast speaker groups and track info.

## New

- Zones replace speaker groups. A zone is a set of speakers with its own volume, and clicking its name turns them on or off together. Your groups become zones once, at the first launch.
- Presets save which speakers are on, their volumes, the zone volumes, the master and the equalizer curve, and bring it all back in one click. They're in the main window, the menu bar panel and Window > Presets, where the first nine have Control-Command-1 to 9.
- Web Remote: control your speakers, zones and presets from a phone on the same network. It's off until you turn it on under Web Remote… in the gear menu or the Window menu. It uses plain HTTP, so keep it to your home network.
- Calibrate All Speakers measures every speaker that's on, one at a time, and sets all their delays as one change you can undo. Calibrate Zone… does the same for one zone.
- A source can be several apps at once, or every app except the ones you check.
- An audio interface with more than two channels gets an Input Channels menu, and a microphone or line in shows a level meter.
- Save your own equalizer curves under a name.
- Google Home speaker groups play as one Cast speaker. Cast speakers show what's playing, follow Master Volume, and can be added by address when a search doesn't find them.
- AirPlay speakers get how far into the track you are, and the buttons on a speaker's remote control the playing app and the speaker's volume in AudioSync.
- AirPlay speakers start from the latency they report, so they line up better before you calibrate.
- Hide Speaker takes a speaker you never use out of the lists. Hidden Speakers in the gear menu brings it back.
- AirPlay 2 has a switch now, "AirPlay 2 (Experimental)" in the app menu or the gear menu. It's still off by default and still untried on a real AirPlay 2 speaker.

## Changed

- A microphone or line in keeps every speaker in step, like any other source. With an AirPlay or Cast speaker on, everything plays about 2 seconds after the input.
- AirPlay speakers can wait up to 20 seconds instead of 3, so a long Cast offset or a big calibration result plays in full.
- A Bluetooth speaker gets the speaker icon instead of headphones, unless its name says it's headphones.
- A Bluetooth headset's hands-free microphone is no longer offered as a source or used for calibration, since recording from it drops the headset into call mode.
- Plainer wording across the app, menus in Title Case, and notices that say what failed and what to do.

## Fixed

- Streaming All System Audio while nothing plays no longer says capture permission is missing.
- Calibration works while the source plays nothing.
- A paused player keeps its track and cover on AirPlay speakers instead of flashing AudioSync's own title.
- A Google Cast offset you set stays put, and a failed Resync reconnects once instead of sitting on Playing over silence.
- The menu bar panel stays readable over any window behind it.

## Install

- Download `AudioSync.dmg` and drag AudioSync to Applications. It needs macOS 15 or later.
- It's not notarized, so macOS blocks the first launch. Open it once, close the warning, then click Open Anyway in System Settings > Privacy & Security. Or run `xattr -dr com.apple.quarantine /Applications/AudioSync.app`.
- Your speaker groups turn into zones the first time 0.8.0 starts. The old groups stay in the settings, so going back to 0.6.0 still has them.

The full list is in [CHANGELOG.md](https://github.com/yassersutojoyo/audiosync/blob/v0.8.0/CHANGELOG.md).
