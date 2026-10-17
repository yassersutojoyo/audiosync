# Development

Notes for building AudioSync yourself and finding your way around the code. For how to send changes, see [`CONTRIBUTING.md`](../CONTRIBUTING.md); for cutting a release, [`RELEASE.md`](../RELEASE.md).

## Building

You need macOS 15 (Sequoia) or later, [Homebrew](https://brew.sh), and the Command Line Tools for Xcode 16 or later (`xcode-select --install`), which bring Swift 6 and the `python3` the build uses.

```bash
git clone https://github.com/yassersutojoyo/audiosync
cd audiosync
./setup.sh
```

`setup.sh` installs the build dependencies with Homebrew (FFmpeg and friends), builds libinotify-kqueue, downloads OwnTone 29.2 (checksum-pinned), applies AudioSync's patches and builds it into `~/.audiosync-build/engine`, and then runs `build.sh`. The first run takes a while.

After that, `./build.sh` is all you need day to day. It builds the app, bundles the engine and every library it needs (so the app runs on Macs without Homebrew), signs it, and installs it to `/Applications`, or `~/Applications` if `/Applications` isn't writable. Without an engine the app still builds and runs, just without AirPlay speakers. If a `brew upgrade` changes the major version of a library the engine was linked against (FFmpeg, say), bundling stops and asks you to run `./setup.sh` again.

The bundled Homebrew libraries need the macOS they were built on or later. `bundle_engine.py` warns when that's newer than the app's own minimum (macOS 15).

A few environment variables change what the scripts do:

- `AUDIOSYNC_INSTALL_DIR`: install somewhere other than `/Applications`
- `AUDIOSYNC_ENGINE`: bundle a different engine binary
- `AUDIOSYNC_ADHOC=1`: sign ad-hoc instead of with the local identity
- `AUDIOSYNC_SKIP_BREW=1`: `setup.sh` skips Homebrew (handy offline)
- `AUDIOSYNC_ENGINE_ONLY=1`: `setup.sh` stops after building the engine
- `CODESIGN_IDENTITY`: sign with a Developer ID, given by its full name (see `RELEASE.md`)
- `ENTITLEMENTS`: entitlements for the app signature (Developer ID builds default to `AudioSync.entitlements`)
- `SPARKLE_DIR`: use an already verified Sparkle folder instead of the cached archive

### Signing

On the first build, `build.sh` runs `make-local-cert.sh`, which creates a self-signed "AudioSync Local Code Signing" identity in its own keychain (`~/Library/Keychains/audiosync-signing.keychain-db`), with no password prompt and no trust settings. Every build is signed with it, so macOS sees each rebuild as the same app and the System Audio Recording permission sticks.

With an ad-hoc signature the code hash changes on every build, and the permission quietly stops working: the toggle in System Settings stays on, but capture is silent. That's why ad-hoc is only a fallback.

## Running and testing

```bash
open -a AudioSync
Tests/run-tests.sh               # ring buffer, logic and FIFO tests, no audio hardware
Tests/run-tests.sh --hardware    # adds a silent engine test on every real output device
```

The tests are standalone Swift files in `Tests/`, no XCTest. Run the `--hardware` variant too when you touch the audio path.

For a debug log, quit AudioSync and start it from Terminal:

```bash
AUDIOSYNC_DEBUG=1 /Applications/AudioSync.app/Contents/MacOS/AudioSync
```

It writes to `/tmp/audiosync-debug.log`.

## Where things are

Everything lives in `Sources/AudioSync`. The audio path, roughly in order:

- `AudioSource.swift`, `ProcessTapCapture.swift`: what to capture (system, one app, an input device) and the capture itself
- `Equalizer.swift`: the 10-band EQ (`EqualizerWindow.swift` is its window)
- `RingBuffer.swift`: the shared delay buffer, one independent reader per speaker
- `OutputRenderer.swift`: one Core Audio output per local speaker
- `SyncEngine.swift`: ties capture, local outputs and the AirPlay pipe together
- `FifoFeed.swift`: real-time 48 kHz PCM16 into the engine's pipe
- `AirPlayEngine.swift`: starts, configures and supervises the bundled OwnTone
- `OwnToneClient.swift`: the client for the engine's local API
- `AirPlayPairing.swift`: speakers the engine switched off, and PIN or password pairing

The app side: `AppDelegate.swift` holds app state, persistence, launch and quit, and its extensions split out windows (`+Windows`), groups and auto-connect (`+Groups`), local outputs (`+Outputs`), AirPlay speakers (`+AirPlay`) and streaming and notices (`+Engine`). The UI is SwiftUI in `MainWindow.swift` and `MenuBar.swift`, with the app menus in `MainMenu.swift`. Smaller pieces: `AudioDevices.swift` (Core Audio device helpers), `Volume.swift` (the loudness curve), `SpeakerGroups.swift`, `NowPlaying.swift` (Spotify and Music), `Diagnostics.swift` (the debug log) and `main.swift` (keeps one copy running).

All user-facing text goes through `AppText` in `Localization.swift`. `Brand.swift` has the brand colors and typeface; `BrandMark.swift` is generated, so don't edit it.

Outside the app: `setup.sh`, `build.sh` and `scripts/bundle_engine.py` build and bundle the engine, `make-dmg.sh` packages a release, and `scripts/archive_sources.sh` gathers library sources for one (see `RELEASE.md`).

## Logo, icons and screenshots

The logo, app icon and wordmark are generated from the sources in `brand/` by `scripts/brand/build-brand.sh`; [`brand/README.md`](../brand/README.md) explains how. Don't edit `AppIcon.icns`, `icon-source.png`, the `site/` icons, `site/wordmark.svg` or `BrandMark.swift` by hand. CI checks the generated SVGs and `BrandMark.swift`, but not the PNG and ICNS files, so regenerate those yourself.

The screenshots in `docs/images` come from `scripts/screenshots/render.sh`. It draws the app's own SwiftUI views offscreen from mock data, in light and dark, so it needs no installed app, engine, permissions or audio devices. Control styling follows the macOS you run it on, so re-render after a UI change.

## Uninstalling

If this is the Mac that builds releases, stop first: removing `~/Library/Application Support/AudioSync` also deletes `signing-keychain.pass`, the password of the release signing keychain. Back up both (see `RELEASE.md`).

Otherwise, turn off Launch at Login in the gear menu, quit AudioSync, and run:

```bash
rm -rf /Applications/AudioSync.app ~/Applications/AudioSync.app \
  ~/Library/Application\ Support/AudioSync \
  ~/Library/Caches/io.github.yassersutojoyo.audiosync \
  ~/Library/HTTPStorages/io.github.yassersutojoyo.audiosync
defaults delete io.github.yassersutojoyo.audiosync
tccutil reset All io.github.yassersutojoyo.audiosync
security delete-keychain ~/Library/Keychains/audiosync-signing.keychain-db   # if you built it yourself
rm -rf ~/.audiosync-build
```

The Homebrew packages `setup.sh` installed stay; `brew uninstall` them if nothing else needs them.
