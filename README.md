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
  A free, open-source Airfoil alternative for macOS: play your Mac's audio on HomePods, AirPlay speakers and every local output at once, in sync.
</p>

<p align="center">
  <a href="https://github.com/yassersutojoyo/audiosync/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/yassersutojoyo/audiosync/actions/workflows/ci.yml/badge.svg?branch=main"></a>
  <a href="https://github.com/yassersutojoyo/audiosync/actions/workflows/windows.yml"><img alt="Windows CI" src="https://github.com/yassersutojoyo/audiosync/actions/workflows/windows.yml/badge.svg?branch=main"></a>
  <img alt="macOS 15+" src="https://img.shields.io/badge/macOS-15%2B-black?logo=apple">
  <img alt="MIT license" src="https://img.shields.io/badge/license-MIT-blue">
  <img alt="Early alpha" src="https://img.shields.io/badge/status-early%20alpha-E3A21A">
</p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/main-window-dark.png">
    <img src="docs/images/main-window-light.png" width="720" alt="AudioSync main window: audio source, now playing, master volume and equalizer on the left; on the right, the MacBook Pro speakers under This Mac, a speaker group, then Connected Speakers listed by name, each with a small AirPlay, USB or wireless icon for its kind, with volume and, for local outputs, a sync delay">
  </picture>
</p>

I built this because I wanted my Mac's audio on every speaker in the house at once. The catch is that a HomePod always plays a little late (AirPlay buffers about two seconds), so a HomePod and a desk speaker playing the same song sound like an echo. AudioSync captures the audio once, streams it to your AirPlay speakers, and plays a delayed copy on each local output, every one with its own offset, until the whole room sounds like one speaker.

It's a small Mac app written in Swift, with a main window and a menu bar panel, free and MIT-licensed. It's an early alpha, so expect rough edges.

A Windows version is in development too, written in Rust. It isn't usable yet.

## Multi-room audio on macOS

The source can be all system audio, a single app (with its helper processes, like a browser's audio service), or an input device such as a microphone, line-in or audio interface (first two channels). It plays to one or more HomePods, Apple TVs and other AirPlay receivers, plus anything macOS can play to: built-in speakers, the headphone jack, HDMI/DisplayPort, USB DACs and Bluetooth.

Each local speaker gets its own delay (0 to 20 s), and the AirPlay engine keeps the AirPlay speakers in step with each other. On top of that there's per-speaker and master volume, a 10-band equalizer with a preamp and presets, Now Playing for Spotify and Music, speaker groups, auto-connect, and launch at login. The AirPlay engine (OwnTone) ships inside the app and AudioSync starts and stops it itself, so there's nothing else to install or configure.

## Coming from Airfoil?

If you've used Airfoil, the idea will feel familiar: send the whole system's audio, or just one app, to AirPlay speakers and the Mac's own outputs at the same time, with each local speaker lined up so nothing echoes. AudioSync is free and open source, but it does less. It only streams to AirPlay receivers and outputs macOS already knows about, so there's no Chromecast or other kinds of network speakers. It needs macOS 15 or later, everything runs about two seconds behind, and it's still an alpha.

To be clear: Airfoil is a product of Rogue Amoeba. AudioSync isn't affiliated with them or endorsed by them, and it shares no code with Airfoil.

## Install

Download `AudioSync.dmg` from the [latest release](https://github.com/yassersutojoyo/audiosync/releases/latest) and drag the app to Applications. It runs on macOS 15 (Sequoia) or later, AirPlay included.

I don't have a paid Apple developer account, so releases aren't notarized and macOS blocks the first launch. Open AudioSync once, dismiss the warning, then go to System Settings → Privacy & Security and click Open Anyway next to the message about AudioSync. (Or run `xattr -dr com.apple.quarantine /Applications/AudioSync.app`.) Each release lists SHA-256 checksums for its files.

Auto-update is off for now. To update, quit AudioSync and replace the app with the one from [Releases](https://github.com/yassersutojoyo/audiosync/releases). [`CHANGELOG.md`](CHANGELOG.md) has what changed.

You can also build it yourself. You need Homebrew and the Xcode 16 command line tools, then:

```bash
git clone https://github.com/yassersutojoyo/audiosync
cd audiosync
./setup.sh
```

The first run takes a while (around ten minutes): it builds the AirPlay engine and its libraries, then installs the app. [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) has the details, plus how to uninstall.

On first use macOS asks for Screen & System Audio Recording (choose System Audio Recording Only) to capture audio, and Local Network to find AirPlay speakers. Microphone access is only needed for an input source, and Automation (Spotify, Music) only for Now Playing.

For HomePods, set Home app → Home Settings → Speakers & TV → Allow Speaker & TV Access to "Anyone on the Same Network", on a network you trust. Avoid "Everyone", which lets any nearby device stream.

## Using it

Leave the macOS sound output on a local device. Don't pick a HomePod in Control Center or in your player; AudioSync streams to AirPlay speakers itself.

The main window opens when AudioSync starts (not when it launches at login; click the Dock icon to bring it back). Pick an audio source, then turn speakers on by clicking their round icons. The list goes:

- **This Mac**: the Mac's own speakers and headphone jack
- your groups
- **Connected Speakers**: AirPlay, Bluetooth, USB, HDMI and every other output, by name, each with a small icon for its kind that turns to color while it plays

The switch at the top turns all streaming on or off. Switching it off keeps your speakers chosen (they show as "will play") and they come back when you switch it on again.

To line things up, click the delay next to a local speaker (like ⏱ 2000 ms). If the local speaker plays first and the HomePod echoes behind it, raise the delay; if the HomePod plays first, lower it. Stop when you hear one clean sound; the value is saved for that device. New devices start at 2000 ms and most setups end up around 1500 to 2000. Music with clear claps or drums makes it easier, and so does adding speakers one at a time. Bluetooth headphones have their own lag, so they need a bit less. The delay only applies while an AirPlay speaker is on; local speakers on their own play right away.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/menu-bar-panel-dark.png">
    <img src="docs/images/menu-bar-panel-light.png" width="328" alt="AudioSync menu bar panel: power switch, audio source, master volume, then a row for the Mac's own speakers, a divider, a group, and the connected speakers by name with a small icon for their kind of output, each with an on/off state, above Open AudioSync and Quit AudioSync">
  </picture>
</p>

The menu bar icon opens a compact panel with the power switch, source, master volume and every speaker and group; click a row to switch it. When something needs attention, like a speaker that got switched off or a source app that quit, a notice shows at the top of the window and the panel, and a dot appears next to the menu bar icon. Notices clear themselves once the cause is gone, or you can dismiss them.

A few other things worth knowing:

- Right-click a speaker (or use its ⋯ button) for auto-connect, and a group to rename or delete it.
- The gear menu has New Group from Active Speakers, Refresh Devices, Show Virtual Devices (loopback drivers, meeting apps and other virtual outputs stay hidden until you turn this on or already use one), Appearance, Launch at Login and the AudioSync Guide.
- Clicking any speaker while streaming is off starts streaming. Click a local speaker's delay to nudge it by ±10/±100 ms or type a value.
- Speakers that want a PIN or password (an Apple TV, or a receiver with Require Password on) are marked "Asks for a code". You get a prompt when you turn one on yourself; after pairing once it connects without a code. Old AirPlay 1 receivers with a password are marked "Password not supported"; turn the password off on the receiver.
- If the AirPlay engine can't start, or stops after repeated crashes, Connected Speakers says so and offers a Retry button.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/equalizer-dark.png">
    <img src="docs/images/equalizer-light.png" width="568" alt="AudioSync equalizer window with ten band sliders, preamp and a preset menu">
  </picture>
</p>

## How it works, briefly

System or app audio comes in through a Core Audio process tap (which also mutes the original output, so you only hear the synced copies); input devices are read directly. The audio goes through the equalizer into a shared buffer. Each local speaker has its own output unit reading that buffer at its own delay and correcting clock drift. The same audio goes through a named pipe into the bundled [OwnTone](https://github.com/owntone/owntone-server) engine, which speaks AirPlay and AirPlay 2.

HomePods can't show a PIN, so AudioSync builds OwnTone with a small patch that tries PIN-less (transient) pairing first, which is why the Allow Speaker & TV Access setting above matters. The engine's API listens on 127.0.0.1 only and needs a password that changes on every start. [`SECURITY.md`](SECURITY.md) goes into the details.

## Known limitations

- Everything is about two seconds late because of the AirPlay buffer. Fine for music, not for video or games.
- AirPlay 2 support in OwnTone is reverse-engineered, so a HomePod or macOS update can break it until OwnTone catches up.
- Now Playing only works with Spotify and Music; macOS doesn't let other apps read the system-wide Now Playing info.

## License and credits

AudioSync's own code is under the [MIT License](LICENSE). The bundled AirPlay engine and its libraries keep their own licenses; as distributed, the engine is under OwnTone's GPL-2.0-or-later. [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) lists everything, including the engine patches and where to get the source.

- AudioSync started as a fork of [HomeSounds Sync](https://github.com/techjuicelab/homesounds-sync) by TechJuicelab. The original copyright notice is kept in `LICENSE`.
- [OwnTone](https://github.com/owntone/owntone-server) (GPL-2.0-or-later) is the AirPlay engine, bundled and run as a separate process.
- [Sparkle](https://sparkle-project.org) (MIT) is the update framework, embedded but currently disabled.
- [Poppins](https://github.com/itfoundry/Poppins) (SIL Open Font License 1.1) is the wordmark typeface, bundled in the app.

AudioSync is an independent project. It is not affiliated with, endorsed by, or based on code from Rogue Amoeba; Airfoil is a trademark of Rogue Amoeba Software, LLC, mentioned only to say what AudioSync is an alternative to. Apple, AirPlay and HomePod are trademarks of Apple Inc. The Bluetooth® word mark and logos are registered trademarks owned by Bluetooth SIG, Inc. AudioSync is not affiliated with or endorsed by Apple or Bluetooth SIG.

Want to build it, hack on it or report something? See [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md), [`CONTRIBUTING.md`](CONTRIBUTING.md) and [`SECURITY.md`](SECURITY.md).
