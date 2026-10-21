<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="brand/audiosync-icon-dark.svg">
    <img src="brand/audiosync-icon.svg" width="200" align="top" alt="AudioSync app icon">
  </picture>
  <br>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="brand/audiosync-wordmark-dark.svg">
    <img src="brand/audiosync-wordmark.svg" width="174" alt="AudioSync">
  </picture>
</p>

<p align="center">
  A free, open-source Airfoil alternative for macOS: play your Mac's audio on AirPlay speakers and every local output at once, in sync.
</p>

<p align="center">
  <a href="https://github.com/yassersutojoyo/audiosync/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/yassersutojoyo/audiosync/actions/workflows/ci.yml/badge.svg?branch=main"></a>
  <a href="https://github.com/yassersutojoyo/audiosync/actions/workflows/windows.yml"><img alt="Windows CI" src="https://github.com/yassersutojoyo/audiosync/actions/workflows/windows.yml/badge.svg?branch=main"></a>
  <img alt="macOS 15+" src="https://img.shields.io/badge/macOS-15%2B-black?logo=apple">
  <img alt="GPL-3.0-or-later license" src="https://img.shields.io/badge/license-GPL--3.0--or--later-blue">
  <img alt="Early alpha" src="https://img.shields.io/badge/status-early%20alpha-E3A21A">
</p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/main-window-dark.png">
    <img src="docs/images/main-window-light.png" width="720" alt="AudioSync main window: audio source, now playing, master volume and equalizer on the left; on the right, the MacBook Pro speakers under This Mac, a speaker group, then Connected Speakers listed by name, each with a small AirPlay, USB or wireless icon for its kind, with volume and, for local outputs, a sync delay">
  </picture>
</p>

I built this because I wanted my Mac's audio on every speaker in the house at once. The catch: AirPlay buffers about two seconds, so an AirPlay speaker and a desk speaker playing the same song sound like an echo. AudioSync captures the audio once, streams it to your AirPlay speakers, and plays a delayed copy on each local output, each with its own offset, until the room sounds like one speaker.

It's a small Mac app with a main window and a menu bar panel, free and open source. It's an early alpha, so expect rough edges. What changed in each version is in [`CHANGELOG.md`](CHANGELOG.md).

## What it does

- The source is all system audio, one app (with its helper processes), or an input device like a mic or audio interface (first two channels).
- It plays to AirPlay speakers and Apple TVs, plus anything macOS can play to: built-in speakers, headphone jack, HDMI, USB DACs, Bluetooth.
- Each local speaker gets its own delay relative to the Mac's built-in speakers (−2 to +20 s), with or without AirPlay on. One more delay lines the Mac up with the AirPlay speakers, which stay in step with each other.
- Per-speaker and master volume, a 10-band equalizer with presets, groups, auto-connect, launch at login, and Now Playing for Spotify and Music (shown on AirPlay TVs too).
- If a speaker you turned on sits at 25% or less in macOS, or is muted, its row says so and one click fixes it.

If you know Airfoil, the idea will feel familiar, but AudioSync does less: AirPlay and the outputs macOS already knows, no Chromecast or other network speakers. It isn't affiliated with Rogue Amoeba and shares no code with Airfoil.

## Install

Download `AudioSync.dmg` from the [latest release](https://github.com/yassersutojoyo/audiosync/releases/latest) and drag the app to Applications. You need macOS 15 (Sequoia) or later.

I don't have a paid Apple developer account, so releases aren't notarized and macOS blocks the first launch. Open it once, dismiss the warning, then click Open Anyway in System Settings → Privacy & Security (or run `xattr -dr com.apple.quarantine /Applications/AudioSync.app`). Each release lists SHA-256 checksums. There's no auto-update: quit AudioSync and replace the app with the new one.

On first use macOS asks for Screen & System Audio Recording (choose System Audio Recording Only) to capture audio, and Local Network to find AirPlay speakers. Microphone is only needed for an input source, Automation only for Now Playing.

For HomePods, set Home app → Home Settings → Speakers & TV → Allow Speaker & TV Access to "Anyone on the Same Network", on a network you trust, not "Everyone".

To build it yourself you need the Xcode 16 command line tools and [rustup](https://rustup.rs), then `git clone https://github.com/yassersutojoyo/audiosync`, `cd audiosync` and `./setup.sh`. [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) has the details and how to uninstall.

## Using it

Keep the macOS sound output on a local device, not a HomePod; AudioSync streams to AirPlay itself. The main window opens at start but not at login; click the Dock icon to bring it back. Pick a source, then click a speaker's round icon to turn it on. The switch at the top turns all streaming on or off and remembers your speakers.

To line things up: if a local speaker plays before the built-in speakers, raise its delay; if after, lower it, below zero if needed (Bluetooth usually lands around −150 to −300 ms). With an AirPlay speaker on, raise the AirPlay delay if the Mac plays first, lower it if the HomePod does. Stop when you hear one clean sound. Music with clear drums helps, and so does adding speakers one at a time.

Right-click a speaker for auto-connect, or a group to rename it. The gear menu has groups, Show Virtual Devices, Launch at Login, Switch to All System Audio When the Source App Quits, and the full AudioSync Guide. Speakers that want a PIN or password are marked "Asks for a code" and prompt you when you turn them on; an Apple TV pairs once.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/menu-bar-panel-dark.png">
    <img src="docs/images/menu-bar-panel-light.png" width="328" alt="AudioSync menu bar panel: power switch, audio source, master volume, then a row for the Mac's own speakers, a divider, a group, and the connected speakers by name with a small icon for their kind of output, each with an on/off state, above Open AudioSync and Quit AudioSync">
  </picture>
</p>

## Known limitations

- Everything runs about two seconds late because of the AirPlay buffer. Fine for music, not for video or games.
- AirPlay goes out as AirPlay 1, which most AirPlay 2 speakers still take. A speaker that only does AirPlay 2 isn't listed yet.
- I haven't tried the new AirPlay sender on a HomePod yet. If yours doesn't show up or play, please [open an issue](https://github.com/yassersutojoyo/audiosync/issues/new/choose) with a debug log: quit AudioSync, run `AUDIOSYNC_DEBUG=1 /Applications/AudioSync.app/Contents/MacOS/AudioSync`, make it happen again, and attach `/tmp/audiosync-debug.log`.
- Several Bluetooth outputs at once may stutter. One Bluetooth radio serves every speaker in turns and shares 2.4 GHz with Wi-Fi. AudioSync keeps clock drift in check, but it can't fix the radio. What helps: 5 GHz Wi-Fi or a cable for the internet, fewer Bluetooth outputs, a dual-link USB transmitter, or AirPlay for the rest.
- Now Playing only works with Spotify and Music; macOS doesn't let other apps read the system-wide Now Playing info.

## Windows

A Windows 11 version (x64 and ARM64) is in the works. It shares the Rust audio engine with the Mac app and has its own AirPlay sender, so no Bonjour or Apple software is needed on the PC. It isn't released yet; CI builds it on every push, and [windows/README.md](windows/README.md#trying-the-preview-exe) says how to try a build. For now it only plays to AirPlay 1 receivers (a password prompts you when you turn one on), and I haven't tried its AirPlay side with a real speaker yet.

## License and credits

Copyright (c) 2026 Yasser Sutojoyo Kusumo. AudioSync is free software under the GPL-3.0-or-later, with no warranty; see [`LICENSE`](LICENSE). What it bundles from others, the Poppins font included, is listed with its license in [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).

Apple, AirPlay and HomePod are trademarks of Apple Inc., Airfoil of Rogue Amoeba Software, LLC, and the Bluetooth® word mark of Bluetooth SIG, Inc. AudioSync isn't affiliated with or endorsed by any of them.

Building, contributing and security reports: [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md), [`CONTRIBUTING.md`](CONTRIBUTING.md) and [`SECURITY.md`](SECURITY.md).

And of course, thank god for Anthropic and Claude.
