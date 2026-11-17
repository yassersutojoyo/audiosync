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
  Play your computer's sound on AirPlay, Google Cast, Bluetooth and every other speaker it knows, all at once and in sync. For macOS, Windows and Linux. A free, open-source Airfoil alternative.
</p>

<p align="center">
  <a href="https://github.com/yassersutojoyo/audiosync/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/yassersutojoyo/audiosync/actions/workflows/ci.yml/badge.svg?branch=main"></a>
  <a href="https://github.com/yassersutojoyo/audiosync/actions/workflows/windows.yml"><img alt="Windows CI" src="https://github.com/yassersutojoyo/audiosync/actions/workflows/windows.yml/badge.svg?branch=main"></a>
  <img alt="macOS 15+" src="https://img.shields.io/badge/macOS-15%2B-black?logo=apple">
  <img alt="Windows 11" src="https://img.shields.io/badge/Windows-11-0078D4">
  <img alt="Linux, glibc 2.39+" src="https://img.shields.io/badge/Linux-glibc%202.39%2B-FCC624?logo=linux">
  <img alt="GPL-3.0-or-later license" src="https://img.shields.io/badge/license-GPL--3.0--or--later-blue">
  <img alt="Beta" src="https://img.shields.io/badge/status-beta-9A5B00">
</p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/main-window-dark.png">
    <img src="docs/images/main-window-light.png" width="720" alt="AudioSync main window on a Mac: the song playing with its art, the audio source, and the speakers, each with a round on/off icon and a volume slider">
  </picture>
</p>

AirPlay speakers play a little late and local speakers don't, so the same song on both turns into an echo. AudioSync grabs your computer's audio once, sends it to the AirPlay speakers and plays a delayed copy on each local speaker, so everything lines up. It's free, and it's a beta, so expect rough edges.

It's one app on the Mac, Windows and Linux. All three run the same Rust core for the audio, the sync and the AirPlay and Google Cast senders; only the window around it differs.

You don't have to find each delay by ear. Calibrate with Microphone plays a short sweep on each speaker, hears when it arrives and sets the delay. On a Mac, 12 runs on one Bluetooth speaker gave offsets within ±1.6 ms of each other.

On AirPlay, AudioSync adds 0.1 s to the wait the speaker asks for, and never more than 0.2 s. It steps up to 0.2 s after a speaker had to skip a bit of audio, in a quiet moment such as the gap between two songs, and comes back to 0.1 s by itself.

## What it does

- A delay per local speaker, from −2 to +20 s, and one for AirPlay, set by hand or by Calibrate with Microphone.
- Source: all system audio, one app, several apps (or every app except a few), or an input like a mic.
- Zones, presets, and a Web Remote that runs in your phone's browser.
- Outputs: AirPlay speakers and Apple TVs, Google Cast speakers, and whatever your computer plays to: built-in, headphones, HDMI, USB, Bluetooth.
- Volume per speaker and master, a 10-band EQ and Now Playing.
- Free and GPL-3.0-or-later. The engine and the senders are my own code; the libraries it uses are in [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).

On the Mac it lives in the menu bar, on Windows it's a window with a tray icon, and on Linux it sits in the tray when your desktop has one (it plays through PipeWire).

Up to 16 AirPlay 2 speakers play at once, and a stereo pair or an Apple TV with its HomePods shows up as one row. Google Cast plays one speaker or Google Home group at a time. Cast has no shared clock, so you set its offset by ear once and it's remembered; Resync under its volume fixes drift.

Not made by Rogue Amoeba, and no shared code with Airfoil.

## Install

macOS 15 or later, Apple silicon:

- Get `AudioSync.dmg` from the [latest release](https://github.com/yassersutojoyo/audiosync/releases/latest) and drag it to Applications. It isn't notarized, so on the first launch click Done, then System Settings → Privacy & Security → Open Anyway.
- Or from Terminal, with no Open Anyway step: `curl -fsSL https://github.com/yassersutojoyo/audiosync/releases/latest/download/install.sh | bash`
- Or with Homebrew, which installs the same DMG: `brew install --cask yassersutojoyo/audiosync/audiosync`

Windows 11, x64 or ARM64:

- Get the zip for your PC from [Releases](https://github.com/yassersutojoyo/audiosync/releases), unzip it into its own folder and run `audiosync.exe`. It isn't signed, so SmartScreen may ask: click More info, then Run anyway. [desktop/README.md](desktop/README.md#running-the-exe) has more.
- Or from PowerShell, no admin and no SmartScreen: `irm https://github.com/yassersutojoyo/audiosync/releases/latest/download/install.ps1 | iex`

Linux, x86_64 or aarch64, glibc 2.39 or newer and PipeWire:

- Get the AppImage from [Releases](https://github.com/yassersutojoyo/audiosync/releases), make it executable and run it.
- Or from a terminal, no sudo: `curl -fsSL https://github.com/yassersutojoyo/audiosync/releases/latest/download/install-linux.sh | bash`
- On Debian or Ubuntu there's a `.deb` too: `sudo apt install ./AudioSync-<version>-amd64.deb` (or `arm64`).
- On Fedora 40 or newer there's an `.rpm` for x86_64: `sudo dnf install ./AudioSync-<version>-x86_64.rpm`.
- Hear the source twice with no output to spare? [desktop/README.md](desktop/README.md#linux-a-silent-output) shows how to add a silent one.

Each install script checks the download against the release's SHA-256 checksums; run it again to update. The Mac app, the Windows exe and the AppImage update themselves; a `.deb` or `.rpm` install and Windows 0.7.1 don't, so get the next version by hand. To build it yourself, see [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md).

## Use

- Leave your computer's sound output on a local device, not a HomePod.
- Pick a source and click a speaker's round icon to turn it on. Speakers that want a code or password ask when you do.
- On the Mac, the window doesn't open at login. Click the Dock icon.
- Speaker too early? Raise its delay. Too late? Lower it, below zero if needed. Bluetooth usually lands around −150 to −300 ms.
- For video, a player with an audio delay setting (mpv, VLC, IINA) can hold the picture back to match.
- Several Bluetooth outputs at once may stutter, since one radio serves them all and shares 2.4 GHz with Wi-Fi. Fewer of them, 5 GHz Wi-Fi or AirPlay for the rest helps.
- The Web Remote is plain HTTP, so I'd keep it off on networks you share with strangers.
- HomePods: in Allow Speaker & TV Access, pick “Anyone on the Same Network” on a network you trust.
- More in Help > AudioSync Guide.
- Something broken? [Open an issue](https://github.com/yassersutojoyo/audiosync/issues/new/choose) with a log. On the Mac, quit AudioSync, run `AUDIOSYNC_DEBUG=1 /Applications/AudioSync.app/Contents/MacOS/AudioSync`, do it again and attach `/tmp/audiosync-debug.log`. On Windows, see [Running the exe](desktop/README.md#running-the-exe).

## License and credits

Copyright (c) 2026 Yasser Sutojoyo Kusumo. GPL-3.0-or-later, no warranty. See [`LICENSE`](LICENSE). Stuff it bundles from others, like the Poppins font, is listed with its license in [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).

Apple, AirPlay and HomePod are trademarks of Apple Inc., Airfoil of Rogue Amoeba Software, LLC, and the Bluetooth® word mark of Bluetooth SIG, Inc. AudioSync isn't affiliated with or endorsed by any of them.

Building, contributing, security: [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md), [`CONTRIBUTING.md`](CONTRIBUTING.md), [`SECURITY.md`](SECURITY.md).

Privacy and signing: [`PRIVACY.md`](PRIVACY.md), [Code signing policy](docs/CODE_SIGNING_POLICY.md).

And of course, thank god for Anthropic and Claude.
