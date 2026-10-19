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

After that, `./build.sh` is all you need day to day. It builds the app, bundles the engine and every library it needs (so the app runs on Macs without Homebrew), signs it, and installs it to `/Applications`, or `~/Applications` if `/Applications` isn't writable, after quitting the running AudioSync. Without an engine the app still builds and runs, just without AirPlay speakers. `./build.sh --no-install` stops before the install: it leaves the signed app in `.build/app/AudioSync.app` and doesn't quit anything. CI builds it that way and checks the bundle with `Tests/check-app-bundle.sh`, which you can also point at an installed copy.

The app links the audio core, the Rust crate in `core/` (see `core/NOTES.md`). `build.sh` first runs `scripts/build_core.sh`, which builds it for Apple silicon and Intel (with rustup, it adds a missing target itself) and joins both into one static library, `core/target/universal-apple-darwin/release/libaudiosync_core.a`. That library exports only the functions in `core/include/audiosync_core.h`, so none of the Rust standard library's own symbols can stand in for the system's. Next to it, `licenses/rust/` gets the notices the app ships for the Rust code inside: the standard library's, and in `crates.txt` those of the crates compiled into the core (`scripts/core_notices.py`, run with `python3`). A plain `swift build`, a scratch build included, links that file: run the script first, or the link stops with "library 'audiosync_core' not found".

Everything is built with `MACOSX_DEPLOYMENT_TARGET=15.0`, the app's `LSMinimumSystemVersion`, so the engine runs on macOS 15 and later, whatever macOS the release is built on. C library functions the SDK marks as newer than 15.0 (`pipe2`, `dup3` and a few others) are reported as missing to `configure`, because a weak import of one is NULL on an older macOS and crashes the engine. `setup.sh` checks the libraries and the engine after building them, and `bundle_engine.py` checks everything it bundles again: the build fails if anything needs a newer macOS than `LSMinimumSystemVersion`, weak-imports from libSystem, or links something outside the app and the system.

A few environment variables change what the scripts do:

- `AUDIOSYNC_INSTALL_DIR`: install somewhere other than `/Applications`
- `AUDIOSYNC_ENGINE_BIN`: bundle a different engine binary (`AUDIOSYNC_ENGINE` with a path still works but is deprecated; its values `rust` and `swift`, from the app's retired engine switch, are ignored)
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

With an ad-hoc signature the code hash changes on every build, and the permission quietly stops working: the toggle in System Settings stays on, but capture is silent. That's why `build.sh` signs ad-hoc only with `AUDIOSYNC_ADHOC=1`, or when there's no local identity and `make-local-cert.sh` can't create one, and says so loudly. If the local identity exists but can't sign, the build stops before it quits or replaces the installed app.

## Running and testing

```bash
open -a AudioSync
Tests/run-tests.sh               # shellcheck, private references, logic and FIFO tests (the FIFO suite also checks the app's wrappers of the core), no audio hardware
Tests/run-tests.sh --hardware    # adds a silent engine test on every real output device and the pipe start test
```

The tests are standalone Swift files in `Tests/`, no XCTest. Run the `--hardware` variant too when you touch the audio path. The script builds the Rust core (`scripts/build_core.sh`) first, since the logic and FIFO suites link it. The logic suite, `Tests/logictest.swift`, checks the app's decisions, the local delay model among them, and builds `AppText` with them, which brings the capture and the engine's parts along. The FIFO suite, `Tests/fifotest.swift`, runs the app's pipe feed against real named pipes, then checks the app's other wrappers of the core (the session's parts, a local output's delay reader driven the way the renderers drive it, the equalizer, the volume curve and gain stage, the AirPlay sync rule). The core's own tests (`cd core && cargo test`) cover the DSP, the delay reader and the rules themselves. The soak rules suite, `Tests/soakrulestest.swift`, checks the soak harness's memory-growth rule (`Tests/soakrules.swift`, see `docs/BENCHMARKS.md`) on made-up footprint series: a run that warms up and then stays flat passes, a steady leak fails, and so does a leak after a spike in the warm-up. `--hardware` runs the engine test, then the pipe start test, `Tests/pipestart.swift`. That one needs no audio device but a built AirPlay engine: it starts a second, private copy of it (the one in `/Applications/AudioSync.app` unless `AUDIOSYNC_PIPESTART_ENGINE` names another binary) in `.build/scratch-pipestart`, with OwnTone's fifo output as its only output and no network access, and times how late audio comes out after streaming pauses, stops and starts again. It leaves the app's own engine alone.

[EDGE_CASES.md](EDGE_CASES.md) lists what AudioSync does when devices, the source, the engine or the network change mid-session (EC1 to EC13), which checks cover each case, and the manual steps for the rest.

The test run starts with `scripts/shellcheck.sh`, which checks every script in `scripts/shellcheck-files.txt` and fails if shellcheck is missing. CI runs the same script with the same pinned shellcheck version (different versions flag different things), so install that version: `brew install shellcheck` gives it today, and `scripts/shellcheck.sh --install <dir>` downloads exactly the pinned release (then put `<dir>` on your `PATH`). A new shell script goes into that list, or the check fails.

`Tests/soak.sh` (an hour of silent playback, judged against a baseline) and `Tests/bench.sh` (writes that baseline) take long, so `run-tests.sh` only compiles their harness. [BENCHMARKS.md](BENCHMARKS.md) explains both.

For a debug log, quit AudioSync and start it from Terminal:

```bash
AUDIOSYNC_DEBUG=1 /Applications/AudioSync.app/Contents/MacOS/AudioSync
```

It writes to `/tmp/audiosync-debug.log`, which starts over at every launch and stops at 50 MB with a line that says so. Each line starts with the time in UTC, like the Windows log; the first one names the app and macOS versions and the Mac's local time.

Every local output that's switched on has its own macOS volume and mute read through Core Audio (`DeviceVolume.swift`), and followed with property listeners until it's switched off or unplugged. The log gets one line when an output is first read (`output <UID> device volume: 13%, not muted; volume settable on the main element, …`), one when what its row shows changes (`fine → low`), and one per click on the fix, with what was written, how each write went and what the device reports afterwards. The only write is that click; nothing else touches a device's own volume.

The Windows app has the same switch. Exit it from the tray icon's menu, then start it from PowerShell in the folder that holds `audiosync.exe`:

```powershell
$env:AUDIOSYNC_DEBUG=1; .\audiosync.exe
```

The log lands in `%TEMP%\audiosync-debug.log` and starts over at every launch; it stops at 50 MB with a line that says so. Each line starts with the time in UTC; the first lines name the app and Windows versions and the PC's local time, so you can line the two clocks up. After that it lists the outputs and sources it found, what you switched, what the session rules decided, every output and capture that started or stopped and why, device changes and errors. For AirPlay it lists the receivers found, changed or gone and whether each can play, every speaker added or removed with its volume and delay, each session's state (connecting, playing, trying again) and errors, the latency each speaker reports, the sender's L, and when the local outputs start or stop waiting for the AirPlay speakers, with every output whose delay changed. While streaming, a status line every five seconds shows the frames captured and, for each output, its delay, volume and how often it ran dry or had to correct drift, and for each AirPlay speaker its packets sent, late and resent. The variable stays set until you close that PowerShell window.

The Windows app keeps the user's choices in `%LOCALAPPDATA%\AudioSync\settings.json`, next to the AirPlay pairings (`windows/src/settings.rs`): the source (whole system, or an app by its executable's file name), each output's switch, volume and delay by endpoint ID, each AirPlay speaker's by the ID it announces, the AirPlay delay adjustment (`airplay_adjustment_ms`, −2000 to +20 000 ms, added to the sender's latency), the master volume, auto-connect marks, groups, the equalizer, View > Show Virtual Devices and the main window's frame. It's plain JSON with a `version` field, now 2: an output's `delay_ms` is its offset relative to the reference output (−2000 to +20 000 ms). Format 1 kept each output's own trim (0 to 20 000 ms) there; a format 1 file is migrated once, when the first output list is in: every other output's trim minus the reference's becomes its offset, the reference's trim becomes the AirPlay adjustment, and the file is saved as format 2 (nothing is saved before that). The debug log notes the migration (`settings: … output delay(s) migrated to offsets relative to …`). The file is read once at launch, before the first device listing, and written at most twice a second (at a poll, when something changed) and once more when the app exits, always through a temporary file that is flushed and renamed over it, so a crash leaves the old file or the new one. A file that's missing, can't be read, isn't valid JSON or comes from a newer version is ignored (the defaults apply, and the next change writes a fresh one); fields it doesn't know are skipped, missing ones take their defaults, and values out of range are clamped. A device that's gone keeps its entry, and the backend gets those settings at launch, so it plays with them when it comes back. The debug log says what happened (`settings: read (format version 2)`, `settings: no file yet …`, `settings: ignored: …`, `settings: saved`). To start over, quit AudioSync and delete the file.

The double-audio notice only shows up while streaming. To look at its guide without playing anything, also set `$env:AUDIOSYNC_PREVIEW_GUIDE="app"` (one app captured) or `"system"` (the whole system) before starting it: with the debug log on, the guide opens at launch for a made-up notice.

To look at the code prompt of an Apple TV pairing without an Apple TV, set `$env:AUDIOSYNC_PREVIEW_PIN="code"` (the TV shows a code) or `"wrong"` (after a wrong code): with the debug log on, the dialog opens at launch for a made-up Apple TV, and nothing is paired.

To check the windows in the other appearance without changing the Windows setting, set `$env:AUDIOSYNC_PREVIEW_THEME="light"` or `"dark"` as well: with the debug log on, every window uses that color scheme, the tray flyout included. The tray icon itself keeps following the taskbar's light or dark mode. The log also says whether each window got its Windows 11 frame (Mica in the title bar, the flyout's rounded corners).

## Where things are

Everything lives in `Sources/AudioSync`. [`ENGINE_SPEC.md`](ENGINE_SPEC.md) describes the engine's behavior clause by clause, and [`EDGE_CASES.md`](EDGE_CASES.md) what happens when devices, sleep or the engine change under a session. [`WINDOWS_AUDIO_ROUTING.md`](WINDOWS_AUDIO_ROUTING.md) weighs how a future Windows version could avoid double audio when capturing an app. The audio path, roughly in order:

- `AudioSource.swift`, `ProcessTapCapture.swift`, `CaptureSupport.swift`: what to capture (system, one app, an input device) and the capture itself
- `Equalizer.swift`: the 10-band EQ's settings and presets (`EqualizerWindow.swift` is its window; the core runs the filters)
- `CoreEngine.swift`: the Rust core's engine parts: the shared delay buffer with one independent reader per speaker, the equalizer, the output gain and the real-time 48 kHz PCM16 feed into the engine's pipe
- `EngineParts.swift`: one session's set of those parts, and what the capture hands each captured block to
- `OutputRenderer.swift`: one Core Audio output per local speaker
- `SyncEngine.swift`: ties capture, local outputs and the AirPlay pipe together
- `LocalDelay.swift`: the local delay model: the reference, the hold-backs each local speaker plays at, the AirPlay delay and the one-time migration of older delays
- `AirPlayEngine.swift`: starts, configures and supervises the bundled OwnTone
- `OwnToneClient.swift`: the client for the engine's local API
- `PipeMetadata.swift`: the track's title, artist, album and artwork, written into the engine's metadata pipe so AirPlay speakers with a screen show them (`ReceiverInfo.swift` decides what they show)
- `AirPlayPairing.swift`: speakers the engine switched off, and PIN or password pairing

The app side: `AppDelegate.swift` holds app state, launch and quit (with its decisions in `AppLifecycle.swift` and the saved settings in `AppSettings.swift`), and its extensions split out windows (`+Windows`), groups and auto-connect (`+Groups`), local outputs (`+Outputs`), AirPlay speakers (`+AirPlay`) and streaming and notices (`+Engine`). The UI is SwiftUI in `MainWindow.swift` and `MenuBar.swift`, with the app menus in `MainMenu.swift`. Smaller pieces: `AudioDevices.swift` (Core Audio device helpers), `Volume.swift` (the loudness curve), `SpeakerGroups.swift`, `NowPlaying.swift` (Spotify and Music), `Diagnostics.swift` (the debug log) and `main.swift` (keeps one copy running).

The app's side of the Rust core (`core/`, imported through the `CAudioSyncCore` module in `Sources/CAudioSyncCore`): `CoreEngine.swift` wraps the core's engine parts, `EngineParts.swift` makes one set of them for each session, and `AudioSyncCore.swift` logs the core's version at launch.

All user-facing text goes through `AppText` in `Localization.swift`. `Brand.swift` turns the design tokens into SwiftUI colors, the typeface and the wordmark; the values themselves live in `brand/tokens.json` and reach the app through the generated `BrandTokens.swift`. `BrandMark.swift` and `BrandTokens.swift` are generated, so don't edit them.

Outside the app: `setup.sh`, `build.sh` and `scripts/bundle_engine.py` build and bundle the engine, `scripts/build_core.sh` builds the Rust core, `make-dmg.sh` packages a release, and `scripts/archive_sources.sh` gathers library sources for one (see `RELEASE.md`).

## The Rust core and the Windows app

Two Rust folders sit next to the Swift package.

`core/` is `audiosync-core`, the platform-independent audio core: the delay ring buffer, drift correction, resampling, the equalizer, volume, the sync policies and PCM16 conversion. It's written from [`ENGINE_SPEC.md`](ENGINE_SPEC.md), and [`core/NOTES.md`](../core/NOTES.md) records how it reads the spec where there was room to choose. The macOS app links it as described under Building. Its tests need only a Rust toolchain, no audio hardware:

```bash
cd core && cargo test
```

`windows/` is a Windows 11 version of AudioSync in development, written in Rust with a [Slint](https://slint.dev) interface. It gets its audio work from the same core: the ring, the delay readers, the renderer with its 20 ms fades, the resampler, the volume curve, the equalizer and the session rules. Every output fades in on its first audio and out before it stops, and a delay change fades out and back in, so starts, stops and source changes don't click. A stop or a source change doesn't hold up the window while the outputs fade: each output's thread keeps its stream running until the device has played the fade (at most 500 ms), the backend's next poll stops what has faded, the old capture last, and a new source's capture starts at the poll after the old one's outputs have faded. It has its screens (the main window, the equalizer, a tray icon with a flyout, the About dialog), and on Windows they drive a WASAPI backend that captures one app or the whole system through process loopback and plays it on the outputs you turn on; on a Mac the window shows example speakers from a mock backend instead. So far it has only been started on one Windows 11 PC, with nothing playing: it runs and lists the outputs, but nobody has listened to it there yet. CI checks the formatting, runs clippy and the tests, and builds it for x64 and ARM64, each build with the license notices `scripts/windows_notices.py` writes for it. On Windows you need rustup with the stable MSVC toolchain and the Visual Studio Build Tools ("Desktop development with C++"), then `cargo test` and `cargo build --release` in `windows/`. On a Mac, `cargo test` runs its logic tests and `cargo check --target x86_64-pc-windows-msvc` checks the Windows code, but can't link it. [`windows/README.md`](../windows/README.md) has the details, and [`WINDOWS_AUDIO_ROUTING.md`](WINDOWS_AUDIO_ROUTING.md) weighs how it could capture one app without playing it twice.

`airplay/` is `audiosync-airplay`, AudioSync's own AirPlay sender, which the Windows app uses for AirPlay speakers (the Mac app keeps its bundled OwnTone engine). It's written from [`AIRPLAY_SPEC.md`](AIRPLAY_SPEC.md), and [`airplay/NOTES.md`](../airplay/NOTES.md) records its readings of the spec. It covers discovery, the AirPlay 1 session and one stream to many speakers, plus Apple TV PIN pairing. The Windows app asks for the code when you switch such an Apple TV on, and keeps the credentials per speaker in `%LOCALAPPDATA%\AudioSync\airplay-pairings.dat`, encrypted with DPAPI for the current Windows user (`windows/src/audio/pairings.rs`); a file it can't read or decrypt is ignored, and those speakers pair again. Its CI checks the formatting, runs clippy and `cargo test` (including a check that the tests cover the spec), and lets in only permissive licenses (`cargo deny check licenses` with `airplay/deny.toml`).

### Running the engine on the Rust core

The app's audio engine runs on the core. Capture (the process tap or input device) and the Core Audio output units stay in Swift, while the delay ring, the per-speaker delay readers with their drift correction, the equalizer, the volume curve and output gain, the AirPlay sync rule and the PCM16 conversion for the AirPlay pipe come from `core/`. Each session gets its own set of the core's parts when streaming starts; should the core fail to make them, streaming doesn't start and the debug log says why. Builds before 2026-10-19 also had the Swift engine the core replaced, chosen by the `engineImpl` setting or `AUDIOSYNC_ENGINE`; both are ignored now, and a launch that finds either one notes it once in the debug log. Of the core's fades (the spec's S182 to S191), only the native AirPlay input's fade-in after each rate set reaches the Mac app so far: its outputs still start and stop without one, nothing asks the pipe feed or the native input to fade out yet, and the pipe writer it creates through `asc_fifo_writer_create` starts with the fade-in off until `asc_fifo_writer_set_fade_in` turns it on.

Local delays follow the local delay model (`ENGINE_SPEC.md` S172 to S181, `LocalDelay.swift`, through the core's `asc_local_*` calls). The reference is the first built-in output whose first output stream reports the speaker terminal type, else the first built-in output, chosen afresh from every device list (`AudioDevices.Device.outputTerminal` is read for built-in outputs only). Every other local speaker has an offset from −2000 to +20 000 ms, stored by device UID under `localOffsetMsByUID`; the core turns the offsets of the speakers that are on and listed into hold-backs, and `retimeLocalOutputs()` in `AppDelegate+Outputs.swift` hands a running renderer a new hold-back only when its own changed. The AirPlay delay is kept per sender: `airPlayDelayMs` for the bundled engine (L itself, 2000 ms by default) and `nativeAirPlayAdjustmentMs` for AudioSync's own sender (added to the latency it reports, 0 by default). Builds before the model stored absolute delays under `localDelayMsByUID` (and an older single one under `delayMs`); the first launch with a device list migrates them once, before auto-connect, sets `localDelayModelMigrated` last, and leaves the old keys alone for older builds. The debug log notes the migration (`delays migrated to offsets relative to …`) and every re-timing (`local delays re-timed: …`).

AudioSync's own AirPlay sender (`airplay/`, the one the Windows app uses) is the Mac app's AirPlay sender too, through the core's C interface (`asc_airplay_*`, wrapped by `NativeAirPlay.swift`). The bundled engine is still built and bundled as a fallback until it's removed: set `airplayImpl` to `owntone` (hold Option while you open the AudioSync menu and untick "Use Native AirPlay (Next Launch)", or run `defaults write io.github.yassersutojoyo.audiosync airplayImpl owntone`) and the next launch uses it again; `native`, or deleting the key, goes back to the default. `AUDIOSYNC_AIRPLAY=native` or `owntone` in the environment wins over the setting. Either way the choice is read once per launch. With the native sender the bundled engine isn't started: the AirPlay rows are the receivers the sender's discovery finds (without this Mac's own AirPlay Receiver), switching one on adds it to the sender while streaming, its volume and the master go to it, the track and artwork from Now Playing go to the receivers that show them (`asc_airplay_set_now_playing`, the same artwork size as the engine's metadata pipe), and its errors show as notices. An Apple TV that asks for a PIN pairs once, when you switch it on, and its credentials are kept in the pairing store (`airplay-pairings.dat` in AudioSync's Application Support folder); a speaker with a password asks for it once per launch. The capture feeds the sender through `NativeAirPlayTap` (from `EngineParts`), which resamples to 44.1 kHz without allocating. While a native speaker is chosen, local speakers wait the sender's latency plus the AirPlay delay adjustment, on top of their offsets (S178); the window's AirPlay delay field shows that sum, and editing it stores the difference to the latency of that moment. Native rows use their own IDs (`native:` and the receiver ID). Speakers saved under the bundled engine's IDs (switched on, volumes, groups, auto-connect) move to those the first time discovery finds their receiver, matched by device ID (`SpeakerIDMigration`, S204); the debug log has a line for each (`settings saved under the bundled engine's ID …`). The debug log lists every receiver found, every speaker added or removed and every state, error and latency (`native AirPlay: …`). If the sender can't start (or can't browse the network), a notice says so and the app tries again every 10 s or so. A bug in the sender doesn't take the app down: the core's release build unwinds, every `asc_airplay_*` call catches a panic (as does the sender's driver thread, through `Sender::is_running`), and a dead sender reports `ASC_AIRPLAY_EVENT_FAILED` once; the AirPlay section then shows it as stopped, with a notice and a Retry button that starts a new one. A panic anywhere else in the core still ends the app, since unwinding out of any other `extern "C"` function aborts.

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
