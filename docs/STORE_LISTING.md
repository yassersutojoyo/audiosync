# Microsoft Store listing (draft)

A draft of the Store listing for the Windows app, in English. Nothing here is submitted yet: that needs a Partner Center account, and the MSIX package is a separate piece of work. Each field gives Microsoft's limit and the current length of the text in its block. Every claim matches what the Windows app does on `main` today, as described in the [README](../README.md), [desktop/README.md](../desktop/README.md), [WINDOWS_UI.md](WINDOWS_UI.md) and the [CHANGELOG](../CHANGELOG.md).

Sources: Microsoft Learn, [Add and edit Store listing info for MSIX app](https://learn.microsoft.com/en-us/windows/apps/publish/publish-your-app/msix/add-and-edit-store-listing-info), [App screenshots, images, and trailers](https://learn.microsoft.com/en-us/windows/apps/publish/publish-your-app/msix/screenshots-and-images), [Categories and subcategories](https://learn.microsoft.com/en-us/windows/apps/publish/publish-your-app/msix/categories-and-subcategories), [Support info](https://learn.microsoft.com/en-us/windows/apps/publish/publish-your-app/msix/support-info) and, for keywords, copyright and "Developed by", whose limits the MSIX pages don't state, [Add additional information for MSI/EXE app](https://learn.microsoft.com/en-us/windows/apps/publish/publish-your-app/msi/add-additional-information). All read on 2026-10-30.

## Store listing

### Product name

Picked from the names reserved in Partner Center; the listing page gives no length limit.

```text
AudioSync
```

### Short description

Limit: 1,000 characters, and Microsoft suggests under 270 because some views cut it there. Current: 249.

```text
Play your PC's sound on AirPlay speakers, Google Cast speakers and the PC's own outputs at the same time. Each speaker has its own delay, so they line up instead of echoing. Free and open source under GPL-3.0-or-later. A preview, still being tested.
```

### Description

Limit: 10,000 characters, plain text, no URLs. Current: 2,272, line breaks included.

```text
AirPlay speakers play about two seconds late. The speakers on your PC don't. Play the same song on both and you hear an echo.

AudioSync captures the sound once, sends it to your AirPlay speakers, and plays a delayed copy on each local speaker, so they all line up. The source can be everything the PC plays, one app, several apps, every app except the ones you pick, or an input device such as a microphone or line in.

What it plays to:
- AirPlay speakers, through AudioSync's own sender. Speakers that only do AirPlay 2, stereo pairs and home theater groups play as they are. A speaker that does both AirPlay 1 and AirPlay 2 uses AirPlay 1 unless you turn on AirPlay 2 (Experimental) in the File menu.
- Google Cast speakers and Google Home speaker groups, one at a time. Cast has no shared clock, so you set its offset by ear once and AudioSync remembers it.
- Any output Windows lists: built-in speakers, the headphone jack, HDMI, USB and Bluetooth.

What you can set:
- A delay per speaker, from -2 to +20 seconds, and one delay for all AirPlay speakers.
- A volume per speaker and a master volume.
- Zones: speakers that turn on and off together, with a volume of their own.
- Presets that recall which speakers are on, their volumes and the equalizer curve.
- A 10-band equalizer, with your own curves saved next to the built-in ones.
- Calibrate with Microphone sets a local speaker's delay for you, and Calibrate All Speakers does every speaker that's on, one after the other.
- Web Remote lets a phone on your home network turn speakers on and off and set volumes and delays.

AudioSync sits in the notification area and can start when you sign in, once you turn that on.

What doesn't work yet:
- Everything is about two seconds late. Fine for music, not for video or games.
- The app you capture keeps playing on its own output too. AudioSync tells you when that happens and shows how to send it to an output you don't listen to, or offers to mute that output when AudioSync doesn't play to it.
- The Windows app hasn't been tried with a real AirPlay speaker yet, and Bluetooth outputs haven't been tried on Windows.
- Web Remote is plain HTTP. Keep it to your home network.

AudioSync is free and open source under GPL-3.0-or-later. The source code and issue tracker are on GitHub.
```

### What's new in this version

Limit: 1,500 characters. Microsoft asks to leave it blank on a first submission.

```text
PLACEHOLDER: leave blank for the first submission. For each update, a short version of that release's CHANGELOG section goes here.
```

### Product features

Limit: up to 20 features, 200 characters each, no bullets of our own. Current: 11 features, the longest 108 characters.

```text
Plays the PC's sound on AirPlay speakers, Google Cast speakers and local outputs at the same time
A delay per speaker, from -2 to +20 seconds, so every speaker lines up
Source: all system audio, one app, several apps, or an input device
Calibrate with Microphone sets a local speaker's delay for you
Calibrate All Speakers measures every speaker that's on, then lets you apply or undo all the results at once
A volume per speaker and a master volume
Zones of speakers that turn on and off together, with their own volume
Presets for which speakers are on, their volumes and the equalizer curve, with Ctrl+1 to Ctrl+9
10-band equalizer with your own saved curves
Web Remote: a phone on your home network turns speakers on and off and sets volumes and delays
Asks for an AirPlay speaker's PIN or password when it needs one
```

### Keywords

Limit: 7 keywords, 40 characters each, 21 unique words across all of them. Not shown to customers. Current: 7 keywords, the longest 24 characters, 12 unique words.

```text
AirPlay
Google Cast
multiroom audio
sync speakers
audio delay
play to several speakers
equalizer
```

### Copyright and trademark info

Limit: 200 characters. Current: 200.

```text
Copyright (c) 2026 Yasser Sutojoyo Kusumo. AirPlay is a trademark of Apple Inc., Google Cast of Google LLC, Bluetooth of Bluetooth SIG, Inc. AudioSync isn't affiliated with or endorsed by any of them.
```

### Additional license terms

Limit: 10,000 characters, or a single URL. Current: 61.

```text
https://github.com/yassersutojoyo/audiosync/blob/main/LICENSE
```

### Developed by

Limit: 255 characters. Current: 22.

```text
Yasser Sutojoyo Kusumo
```

### Additional system requirements

Limit: up to 11 items each for minimum and recommended hardware, 200 characters each. Current: 2 items, the longest 89 characters. Minimum hardware:

```text
An AirPlay or Google Cast speaker on the same network, or a second audio output on the PC
A microphone, only for Calibrate with Microphone
```

## Properties

### Category

Music ("apps for listening, recording, creating, or performing music"), no subcategory exists for it. Secondary category: Utilities + tools.

### System requirements

Windows 11 (build 22000 or later), x64 only. Older versions get a message and the app exits.

### Privacy policy URL

Required when the app accesses, collects or transmits personal information, and Partner Center asks for one by itself when the package declares capabilities that could. The package declares `microphone`, `privateNetworkClientServer` (speakers and Web Remote on the home network) and `runFullTrust`, and not `internetClient`. The repository and the site have no privacy policy page yet.

TODO (maintainer): privacy policy URL.

### Website

```text
https://getaudiosync.vercel.app
```

### Support contact info

```text
https://github.com/yassersutojoyo/audiosync/issues
```

## Screenshots

Limit: at least 1, up to 10 desktop screenshots, PNG, 1366 x 768 or larger, at most 50 MB each, captions up to 200 characters. Microsoft suggests at least four and asks for no extra logos or marketing text on them.

The images in `docs/images` show the Mac app (rendered from mock data by `scripts/screenshots/render.sh`, recorded in [ASSETS.md](ASSETS.md)), so none of them fits a Windows listing. These Windows shots are still needed, each recorded in `docs/ASSETS.md` once taken. Any shot that shows a network address or QR code uses a placeholder address, not a real one.

1. PLACEHOLDER: main window, light theme, with a few speakers on at different delays. Caption: "Each speaker has its own delay and volume."
2. PLACEHOLDER: main window, dark theme, with a zone open. Caption: "Zones turn several speakers on and off together."
3. PLACEHOLDER: tray flyout open over the taskbar. Caption: "Source, master volume and speakers from the notification area."
4. PLACEHOLDER: Equalizer window with a curve picked. Caption: "A 10-band equalizer for every speaker at once."
5. PLACEHOLDER: Calibrate All Speakers results. Caption: "Calibrate All Speakers measures each speaker with a microphone."
6. PLACEHOLDER: Web Remote dialog. Caption: "Web Remote: turn speakers on and off from a phone on your home network."
