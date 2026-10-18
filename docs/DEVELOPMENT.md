# Development

Notes for building AudioSync yourself and finding your way around the code. For how to send changes, see [`CONTRIBUTING.md`](../CONTRIBUTING.md); for cutting a release, [`RELEASE.md`](../RELEASE.md).

## Building

You need macOS 15 (Sequoia) or later, [Homebrew](https://brew.sh) (only for the build tools `pkg-config` and `cmake`, plus `nasm` on Intel Macs), the Command Line Tools for Xcode 16 or later (`xcode-select --install`), which bring Swift 6 and the `python3` the build uses, and a Rust toolchain for the audio core in `core/`: install it with [rustup](https://rustup.rs).

The first build needs network access: it adds the two Apple targets to your Rust toolchain and fetches the crates.io index for the core's test-only dependencies. Later builds work offline.

```bash
git clone https://github.com/yassersutojoyo/audiosync
cd audiosync
./setup.sh
```

`setup.sh` installs those build tools with Homebrew, then builds every library the engine links (FFmpeg, libgcrypt, SQLite, libinotify-kqueue and the rest, listed with their versions and checksums in `scripts/engine_deps.sh`) from pinned upstream release tarballs into `~/.audiosync-build/deps`. FFmpeg is an LGPL build without external libraries. Then it downloads OwnTone 29.2 (checksum-pinned), applies AudioSync's patches, builds it against those libraries into `~/.audiosync-build/engine`, and runs `build.sh`. The libraries take about nine minutes the first time; after that they're reused until `scripts/engine_deps.sh` or the target macOS changes. No Homebrew library ends up in the app.

After that, `./build.sh` is all you need day to day. It builds the app, bundles the engine and every library it needs (so the app runs on Macs without Homebrew), signs it, and installs it to `/Applications`, or `~/Applications` if `/Applications` isn't writable. Without an engine the app still builds and runs, just without AirPlay speakers.

The app links the audio core, the Rust crate in `core/` (see `core/NOTES.md`). `build.sh` first runs `scripts/build_core.sh`, which builds it for Apple silicon and Intel (with rustup, it adds a missing target itself) and joins both into one static library, `core/target/universal-apple-darwin/release/libaudiosync_core.a`. That library exports only the functions in `core/include/audiosync_core.h`, so none of the Rust standard library's own symbols can stand in for the system's. A plain `swift build`, a scratch build included, links that file: run the script first, or the link stops with "library 'audiosync_core' not found".

Everything is built with `MACOSX_DEPLOYMENT_TARGET=15.0`, the app's `LSMinimumSystemVersion`, so the engine runs on macOS 15 and later, whatever macOS the release is built on. C library functions the SDK marks as newer than 15.0 (`pipe2`, `dup3` and a few others) are reported as missing to `configure`, because a weak import of one is NULL on an older macOS and crashes the engine. `setup.sh` checks the libraries and the engine after building them, and `bundle_engine.py` checks everything it bundles again: the build fails if anything needs a newer macOS than `LSMinimumSystemVersion`, weak-imports from libSystem, or links something outside the app and the system.

A few environment variables change what the scripts do:

- `AUDIOSYNC_INSTALL_DIR`: install somewhere other than `/Applications`
- `AUDIOSYNC_ENGINE`: bundle a different engine binary
- `AUDIOSYNC_ADHOC=1`: sign ad-hoc instead of with the local identity
- `AUDIOSYNC_SKIP_BREW=1`: `setup.sh` skips Homebrew (handy offline, once the Rust core has been built at least once)
- `AUDIOSYNC_ENGINE_ONLY=1`: `setup.sh` stops after building the engine
- `AUDIOSYNC_BUILD_DIR`: build the libraries and the engine somewhere other than `~/.audiosync-build` (for a test build that leaves your usual engine alone; `build.sh` and `archive_sources.sh` follow it too)
- `AUDIOSYNC_ALLOW_NEWER_ENGINE=1`: `bundle_engine.py` only warns when the engine needs a newer macOS or has libraries it can't trace to `setup.sh`, for a local build deliberately linked against Homebrew. AirPlay then won't start on older systems, so never use it for a release
- `CODESIGN_IDENTITY`: sign with a Developer ID, given by its full name (see `RELEASE.md`)
- `ENTITLEMENTS`: entitlements for the app signature (Developer ID builds default to `AudioSync.entitlements`)
- `SPARKLE_DIR`: use an already verified Sparkle folder instead of the cached archive

### Signing

On the first build, `build.sh` runs `make-local-cert.sh`, which creates a self-signed "AudioSync Local Code Signing" identity in its own keychain (`~/Library/Keychains/audiosync-signing.keychain-db`), with no password prompt and no trust settings. Every build is signed with it, so macOS sees each rebuild as the same app and the System Audio Recording permission sticks.

With an ad-hoc signature the code hash changes on every build, and the permission quietly stops working: the toggle in System Settings stays on, but capture is silent. That's why ad-hoc is only a fallback.

## Running and testing

```bash
open -a AudioSync
Tests/run-tests.sh               # shellcheck, ring buffer, logic and FIFO tests, no audio hardware
Tests/run-tests.sh --hardware    # adds a silent engine test on every real output device and the pipe start test
```

The tests are standalone Swift files in `Tests/`, no XCTest. Run the `--hardware` variant too when you touch the audio path. The script also builds the Rust core (`scripts/build_core.sh`) and runs the parity suite, `Tests/paritytest.swift`: the ring buffer, logic and FIFO suites' checks on both engines' parts (see "Running the engine on the Rust core" below). The core's delay readers go through those checks twice, once with the delay passed to each read and once the way the app's renderers drive them (`setDelay(ms:)`, then `readAtTarget`). Anything the core doesn't match yet prints as KNOWN: it doesn't fail the run until the core gets it right, and then it does, so the check gets tightened. `--hardware` runs the engine test once on each engine, then the pipe start test, `Tests/pipestart.swift`. That one needs no audio device but a built AirPlay engine: it starts a second, private copy of it (the one in `/Applications/AudioSync.app` unless `AUDIOSYNC_PIPESTART_ENGINE` names another binary) in `.build/scratch-pipestart`, with OwnTone's fifo output as its only output and no network access, and times how late audio comes out after streaming pauses, stops and starts again, through both pipe feeds. It leaves the app's own engine alone.

[EDGE_CASES.md](EDGE_CASES.md) lists what AudioSync does when devices, the source, the engine or the network change mid-session (EC1 to EC12), which checks cover each case, and the manual steps for the rest.

The test run starts with `scripts/shellcheck.sh`, which checks every script in `scripts/shellcheck-files.txt` and fails if shellcheck is missing. CI runs the same script with the same pinned shellcheck version (different versions flag different things), so install that version: `brew install shellcheck` gives it today, and `scripts/shellcheck.sh --install <dir>` downloads exactly the pinned release (then put `<dir>` on your `PATH`). A new shell script goes into that list, or the check fails.

`Tests/soak.sh` (an hour of silent playback, judged against a baseline) and `Tests/bench.sh` (writes that baseline) take long, so `run-tests.sh` only compiles their harness. [BENCHMARKS.md](BENCHMARKS.md) explains both.

For a debug log, quit AudioSync and start it from Terminal:

```bash
AUDIOSYNC_DEBUG=1 /Applications/AudioSync.app/Contents/MacOS/AudioSync
```

It writes to `/tmp/audiosync-debug.log`.

## Where things are

Everything lives in `Sources/AudioSync`. [`ENGINE_SPEC.md`](ENGINE_SPEC.md) describes the engine's behavior clause by clause, and [`EDGE_CASES.md`](EDGE_CASES.md) what happens when devices, sleep or the engine change under a session. [`WINDOWS_AUDIO_ROUTING.md`](WINDOWS_AUDIO_ROUTING.md) weighs how a future Windows version could avoid double audio when capturing an app. The audio path, roughly in order:

- `AudioSource.swift`, `ProcessTapCapture.swift`: what to capture (system, one app, an input device) and the capture itself
- `Equalizer.swift`: the 10-band EQ (`EqualizerWindow.swift` is its window)
- `RingBuffer.swift`: the shared delay buffer, one independent reader per speaker
- `OutputRenderer.swift`: one Core Audio output per local speaker
- `SyncEngine.swift`: ties capture, local outputs and the AirPlay pipe together
- `FifoFeed.swift`: real-time 48 kHz PCM16 into the engine's pipe
- `AirPlayEngine.swift`: starts, configures and supervises the bundled OwnTone
- `OwnToneClient.swift`: the client for the engine's local API
- `PipeMetadata.swift`: the track's title, artist, album and artwork, written into the engine's metadata pipe so AirPlay speakers with a screen show them (`ReceiverInfo.swift` decides what they show)
- `AirPlayPairing.swift`: speakers the engine switched off, and PIN or password pairing

The app side: `AppDelegate.swift` holds app state, persistence, launch and quit, and its extensions split out windows (`+Windows`), groups and auto-connect (`+Groups`), local outputs (`+Outputs`), AirPlay speakers (`+AirPlay`) and streaming and notices (`+Engine`). The UI is SwiftUI in `MainWindow.swift` and `MenuBar.swift`, with the app menus in `MainMenu.swift`. Smaller pieces: `AudioDevices.swift` (Core Audio device helpers), `Volume.swift` (the loudness curve), `SpeakerGroups.swift`, `NowPlaying.swift` (Spotify and Music), `Diagnostics.swift` (the debug log) and `main.swift` (keeps one copy running).

The app's side of the Rust core (`core/`, imported through the `CAudioSyncCore` module in `Sources/CAudioSyncCore`): `CoreEngine.swift` wraps the core's engine parts, `EngineParts.swift` has the protocols both engines' parts implement and picks one engine's set for each session, `EngineImpl.swift` reads the setting that chooses, and `AudioSyncCore.swift` logs the core's version at launch.

All user-facing text goes through `AppText` in `Localization.swift`. `Brand.swift` turns the design tokens into SwiftUI colors, the typeface and the wordmark; the values themselves live in `brand/tokens.json` and reach the app through the generated `BrandTokens.swift`. `BrandMark.swift` and `BrandTokens.swift` are generated, so don't edit them.

Outside the app: `setup.sh`, `build.sh` and `scripts/bundle_engine.py` build and bundle the engine, `scripts/build_core.sh` builds the Rust core, `make-dmg.sh` packages a release, and `scripts/archive_sources.sh` gathers library sources for one (see `RELEASE.md`).

## The Rust core and the Windows app

Two Rust folders sit next to the Swift package.

`core/` is `audiosync-core`, the platform-independent audio core: the delay ring buffer, drift correction, resampling, the equalizer, volume, the sync policies and PCM16 conversion. It's written from [`ENGINE_SPEC.md`](ENGINE_SPEC.md), and [`core/NOTES.md`](../core/NOTES.md) records how it reads the spec where there was room to choose. The macOS app links it as described under Building. Its tests need only a Rust toolchain, no audio hardware:

```bash
cd core && cargo test
```

`windows/` is a Windows 11 version of AudioSync in development, written in Rust with a [Slint](https://slint.dev) interface. It gets its audio work from the same core: the ring, the delay readers, the resampler, the volume curve, the equalizer and the session rules. It has its screens (the main window, the equalizer, a tray icon with a flyout, the About dialog), and on Windows they drive a WASAPI backend that captures one app or the whole system through process loopback and plays it on the outputs you turn on; on a Mac the window shows example speakers from a mock backend instead. It hasn't been tried on real Windows hardware yet: CI checks the formatting, runs clippy and the tests, and builds it for x64 and ARM64, each build with the license notices `scripts/windows_notices.py` writes for it. On Windows you need rustup with the stable MSVC toolchain and the Visual Studio Build Tools ("Desktop development with C++"), then `cargo test` and `cargo build --release` in `windows/`. On a Mac, `cargo test` runs its logic tests and `cargo check --target x86_64-pc-windows-msvc` checks the Windows code, but can't link it. [`windows/README.md`](../windows/README.md) has the details, and [`WINDOWS_AUDIO_ROUTING.md`](WINDOWS_AUDIO_ROUTING.md) weighs how it could capture one app without playing it twice.

### Running the engine on the Rust core

The app's audio engine runs on the Swift modules by default. The `engineImpl` setting runs it on the core instead: capture (the process tap or input device) and the Core Audio output units stay in Swift, while the delay ring, the per-speaker delay readers with their drift correction, the equalizer, the volume curve and output gain, the AirPlay sync rule and the PCM16 conversion for the AirPlay pipe come from `core/`. To switch, hold Option while you open the AudioSync menu and pick "Use Rust Audio Engine (Next Start)", or run `defaults write io.github.yassersutojoyo.audiosync engineImpl rust` (`swift` switches back; any other value counts as `swift`). `AUDIOSYNC_ENGINE=rust` or `swift` in the environment wins over the setting, which is how the tests pick an engine. The choice is read when streaming starts, so a change applies the next time it starts, and the debug log names each session's engine (`session engine: rust (setting)`). One known difference so far: if capture stalls and you change a speaker's delay by less than the 35 ms drift dead band, the Swift engine applies the change once audio comes back, but the core loses it, and that speaker stays off by the change until its delay changes again or it restarts. `Tests/soak.sh` and `Tests/bench.sh` take `--engine` too ([BENCHMARKS.md](BENCHMARKS.md)).

## Logo, icons and screenshots

The logo, app icon and wordmark are generated from the sources in `brand/` by `scripts/brand/build-brand.sh`; [`brand/README.md`](../brand/README.md) explains how. Don't edit `AppIcon.icns`, `icon-source.png`, the `site/` icons, `site/wordmark.svg`, `BrandMark.swift` or the Windows icons in `windows/res/` (from `scripts/brand/windows-icons.sh`) by hand.

The design tokens (colors for light and dark, the typeface, spacing, corner radii and opacities) live in `brand/tokens.json`. `python3 -I scripts/brand/tokens.py` writes them into `Sources/AudioSync/BrandTokens.swift` for the Mac app, `windows/ui/tokens.slint` and `windows/src/tokens.rs` for the Windows app and its contrast test, and the CSS variables between the `tokens:begin` and `tokens:end` comments in `site/index.html`; `--check` only reports what's out of date. Change a token in the JSON and rerun the script, never the outputs.

CI regenerates the brand SVGs, `BrandMark.swift` and every token output and fails on any difference, but it doesn't check the PNG, ICNS and ICO files (including `windows/res/`), so regenerate those yourself.

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

The Homebrew build tools `setup.sh` installed (`pkg-config`, `cmake`, plus `nasm` on Intel Macs) stay; `brew uninstall` them if nothing else needs them.
