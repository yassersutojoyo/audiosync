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
  Multiroom audio for your Mac: play its sound on AirPlay speakers, Bluetooth speakers and every other output macOS knows, plus Google Cast, all at once and in sync. A free, open-source Airfoil alternative.
</p>

<p align="center">
  <a href="https://github.com/yassersutojoyo/audiosync/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/yassersutojoyo/audiosync/actions/workflows/ci.yml/badge.svg?branch=main"></a>
  <a href="https://github.com/yassersutojoyo/audiosync/actions/workflows/windows.yml"><img alt="Windows CI" src="https://github.com/yassersutojoyo/audiosync/actions/workflows/windows.yml/badge.svg?branch=main"></a>
  <img alt="macOS 15+" src="https://img.shields.io/badge/macOS-15%2B-black?logo=apple">
  <img alt="GPL-3.0-or-later license" src="https://img.shields.io/badge/license-GPL--3.0--or--later-blue">
  <img alt="Alpha" src="https://img.shields.io/badge/status-alpha-E3A21A">
</p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/main-window-dark.png">
    <img src="docs/images/main-window-light.png" width="720" alt="AudioSync main window: audio source, now playing, master volume and equalizer on the left; on the right, the MacBook Pro speakers under This Mac, a group of two speakers, then Connected Speakers listed by name, each with a small AirPlay, USB or Bluetooth icon for its kind, with volume and, for local outputs, a sync delay">
  </picture>
</p>

AirPlay speakers play about two seconds late. Local speakers don't. Play the same song on both and you get an echo.

AudioSync fixes that. It grabs your Mac's audio once, sends it to your AirPlay speakers, and plays a delayed copy on each local speaker, so they all line up. Mac app, menu bar panel, free, early alpha.

## Coming in 0.8

On `main` now or on the way, in the Mac app and the Windows build that's still in preview:

- Web Remote: turn it on and a phone on your home network can switch speakers on and off and set volumes and delays. It's plain HTTP, so keep it at home.
- Calibrate All Speakers measures the speakers that are on, one after the other, and lets you apply or undo the new delays in one go. It leaves out headphones and virtual outputs, since the microphone can't hear them.
- Zones instead of groups: a zone has its own volume, one click turns its speakers on or off, and Calibrate Zone measures just that zone. Your groups turn into zones by themselves.
- Presets: save which speakers are on with their volumes and EQ curve, and recall it all in one click or with a shortcut.
- Several apps as one source, or every app except the ones you check.
- Audio interfaces with more than two inputs: pick which channel pair plays, under Input Channels.
- A mic or line in works as a source, and a level meter under the source shows it's coming in. With only local speakers on, the delay stays low. Turn on an AirPlay or Cast speaker and every speaker plays in step, about 2 seconds after the input.
- Save your own EQ curves next to the presets.
- The Web Remote can switch the source, zones and presets too.
- On the Mac, a Bluetooth headset's call microphone isn't offered as a source anymore, since using it drops the headset to call quality.
- Calibrate with Microphone works on Windows too.
- Master Volume scales Google Cast speakers and groups too, on the device's own volume.
- A Cast speaker that doesn't show up can be added by its IP address or host name, with Add by Address… under Connected Speakers on the Mac or in the Speakers menu on Windows.
- Cast speakers show the track that's playing, with the title, artist, album and cover.
- On Windows you can rename a speaker too, with a double-click on its name.
- Hide Speaker takes a speaker you never use out of the list, and Hidden Speakers brings it back.

## New in 0.6

- Google Cast speakers, on the Mac and on Windows, one at a time. You set each one's offset by ear, and Resync restarts it if it drifts.
- Cast and AirPlay speakers together: the side that's ahead waits for the other, AirPlay up to 3 s.
- On the Mac, Calibrate with Microphone sets a local speaker's delay for you.
- On the Mac, rename a speaker with a double-click on its name.
- If the app you capture quits, AudioSync can switch to all system audio instead of stopping.
- Local speakers with a clock that runs a bit fast or slow stay in time with smaller corrections.
- Diagrams in the Guide.
- AirPlay 2 is in there, but experimental and off. See below.

The rest is in [`CHANGELOG.md`](CHANGELOG.md).

## What it does

- Source: all system audio, one app, or an input device (mic, audio interface).
- Outputs: AirPlay speakers, Apple TVs, Google Cast speakers, and anything macOS can play to (built-in, headphone jack, HDMI, USB, Bluetooth).
- A delay per local speaker (−2 to +20 s), plus one AirPlay delay.
- Volume per speaker and master, a 10-band EQ, speaker groups that switch on and off together (zones with a volume of their own from 0.8), auto-connect, launch at login, Now Playing for Spotify and Music.

Like Airfoil, but less. Not made by Rogue Amoeba, no shared code.

Google Cast speakers work in both the Mac and the Windows app, one at a time. Cast has no shared clock, so AudioSync can't sync them by itself: you set an offset by ear for each Cast speaker and it's remembered. If one drifts, hit Resync under its volume. That restarts the stream with a short gap, and it never happens on its own. In 0.6, speaker groups made in the Google Home app can't play. From 0.8 a group plays as one speaker, with one volume and one offset, and its row names the members once the group reports them. A speaker in the group keeps its own row and is busy while the group plays. When a Cast speaker and AirPlay speakers are on together, the faster side waits for the slower one, but AirPlay can only wait so long: 3 s in 0.6, 20 s from 0.8. Past that a notice tells you how far ahead AirPlay plays.

## Install

- Get `AudioSync.dmg` from the [latest release](https://github.com/yassersutojoyo/audiosync/releases/latest), open it and drag AudioSync to Applications. Needs macOS 15 or later.
- The first launch takes one extra step, because AudioSync isn't notarized. When macOS says it can't verify the app, click Done, then go to System Settings → Privacy & Security and click Open Anyway. After that it opens like any other app. Only Apple's paid Developer Program would remove that step, and I'm not in it.
- Or, from Terminal: `curl -fsSL https://raw.githubusercontent.com/yassersutojoyo/audiosync/main/scripts/install.sh | bash`. It downloads the latest release's zip, checks it against the release's SHA-256 checksums, puts AudioSync in Applications and opens it. A copy downloaded by curl isn't flagged as downloaded, so macOS doesn't ask for the extra step. Run it again to update.
- Each release lists SHA-256 checksums. No auto-update: replace the app to update.
- macOS asks for Screen & System Audio Recording (choose System Audio Recording Only) and Local Network. Microphone only for an input source or Calibrate with Microphone, Automation only for Now Playing.
- HomePods: in the Home app, set Allow Speaker & TV Access to "Anyone on the Same Network", on a network you trust. Not "Everyone".
- Build it yourself: Xcode 16 command line tools, [rustup](https://rustup.rs), then `./setup.sh`. See [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md).

## Use

- Leave the macOS sound output on a local device, not a HomePod.
- Pick a source, click a speaker's round icon to turn it on.
- The window doesn't open at login. Click the Dock icon.
- Speaker plays too early? Raise its delay. Too late? Lower it, below zero if needed. Bluetooth usually ends up around −150 to −300 ms.
- Mac plays before the HomePod? Raise the AirPlay delay. After? Lower it.
- Speakers that want a PIN or password ask for it when you turn them on. An Apple TV pairs once.
- More in the AudioSync Guide, in the gear menu.

## What doesn't work (yet)

- Everything is about two seconds late. Fine for music, bad for video and games.
- AirPlay 1 only by default. AirPlay 2 is experimental and off: in 0.6, start AudioSync with `AUDIOSYNC_AIRPLAY2=1` to try it. From 0.8 there's a switch instead: turn on AirPlay 2 (Experimental) in the app menu or the gear menu, then quit and reopen AudioSync. It plays one AirPlay 2 speaker at a time, and so far I've only tried it on one AirPlay 2 TV. From 0.8, with the switch on, a stereo pair, or an Apple TV with the HomePods it plays through, shows up as one speaker with one volume and one delay. Both apps show that row. No real pair has been tried yet.
- Not tried on a HomePod yet. If yours fails, [open an issue](https://github.com/yassersutojoyo/audiosync/issues/new/choose) with a log: quit AudioSync, run `AUDIOSYNC_DEBUG=1 /Applications/AudioSync.app/Contents/MacOS/AudioSync`, do it again, attach `/tmp/audiosync-debug.log`.
- Several Bluetooth outputs at once may stutter. One Bluetooth radio serves every speaker in turns and shares 2.4 GHz with Wi-Fi. AudioSync handles clock drift, but it can't fix the radio. What helps: 5 GHz Wi-Fi or a cable for the internet, fewer Bluetooth outputs, a dual-link USB transmitter, or AirPlay for the rest.
- On the Mac, Now Playing only works with Spotify and Music.

## Windows

A Windows 11 version (x64 and ARM64) is on the way, with the same Rust engine and its own AirPlay sender. Not released yet. CI builds it, and [desktop/README.md](desktop/README.md#trying-the-preview-exe) says how to try a build. It plays to AirPlay 1 and Google Cast speakers (AirPlay 2 behind the same switch as on the Mac, there in the File menu), it asks for AirPlay passwords, and it hasn't been tried on a real speaker yet.

## Linux

There's a Linux preview too, built from the same code as the Windows app. Not released yet. CI builds an x86_64 AppImage (glibc 2.39 or newer, PipeWire from your system), and [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md#the-linux-build-and-its-appimage) has the details. It captures the whole system or one app through PipeWire and plays to AirPlay 1 and Google Cast speakers (AirPlay 2 behind the same switch as on Windows) and to your local PipeWire outputs. Like the Windows build, it hasn't been tried on a real speaker yet. It sits in the tray when your desktop has a StatusNotifier host, and runs as a plain window when it doesn't. Its data lives in `$XDG_DATA_HOME/audiosync`, launch at login is an XDG autostart entry, and now playing comes from MPRIS players.

If you hear the source twice and have no output to spare, a PipeWire null sink gives you one that plays nothing out loud. For this session only (it's gone after a restart, or with `pactl unload-module` and the number it printed):

```sh
pactl load-module module-null-sink sink_name=audiosync-silent sink_properties=device.description=Silent
```

To keep it, put this in `~/.config/pipewire/pipewire.conf.d/silent.conf` and restart PipeWire:

```
context.objects = [
    { factory = adapter
        args = {
            factory.name     = support.null-audio-sink
            node.name        = "audiosync-silent"
            node.description = "Silent"
            media.class      = "Audio/Sink"
            audio.position   = "FL,FR"
        }
    }
]
```

Then send the app to Silent in pavucontrol (or make Silent the default for All System Audio). AudioSync sees it as a virtual output and keeps it out of the list until you pick View > Show Virtual Devices; you don't need to turn it on in AudioSync. The commands come from PipeWire's docs for [module-null-sink](https://docs.pipewire.org/page_pulse_module_null_sink.html) and [pipewire.conf](https://docs.pipewire.org/page_man_pipewire_conf_5.html) and its stock config's null-audio-sink example. I haven't tried this on a Linux machine yet.

## License and credits

Copyright (c) 2026 Yasser Sutojoyo Kusumo. GPL-3.0-or-later, no warranty. See [`LICENSE`](LICENSE). Stuff it bundles from others, like the Poppins font, is listed with its license in [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).

Apple, AirPlay and HomePod are trademarks of Apple Inc., Airfoil of Rogue Amoeba Software, LLC, and the Bluetooth® word mark of Bluetooth SIG, Inc. AudioSync isn't affiliated with or endorsed by any of them.

Building, contributing, security: [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md), [`CONTRIBUTING.md`](CONTRIBUTING.md), [`SECURITY.md`](SECURITY.md).

And of course, thank god for Anthropic and Claude.
