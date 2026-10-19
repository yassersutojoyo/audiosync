# AudioSync for Windows

This folder is the start of a Windows 11 version of AudioSync, written in Rust
with a [Slint](https://slint.dev) interface. The audio work comes from
`audiosync-core` in [`../core`](../core), the crate the macOS app links too.

**Status: in development, ready for a first try on a real PC.** It has the Mac
app's screens (the main window, the equalizer, a tray icon with a flyout, the
About dialog), and on Windows they drive the real audio backend: it captures
one app, or everything the PC plays except AudioSync itself, and plays it on
the outputs you turn on, each at its own delay and volume, through the
equalizer. AirPlay speakers on your network show up under Connected Speakers,
through AudioSync's own sender ([`../airplay`](../airplay)); that part hasn't
been tried with a real speaker yet. Nothing is saved: the speakers you turned
on, their delays and volumes, groups and the equalizer start over at every
launch. So far it has only been started on one Windows 11
PC, with nothing playing, and nobody has listened to it there yet; CI builds and
tests it. [docs/WINDOWS_UI.md](../docs/WINDOWS_UI.md) maps each
part of the Mac main window to its Windows counterpart and lists the keyboard
shortcuts.

## What you need

- Windows 11 (build 22000 or later) on x64 or ARM64. On an older Windows the
  app says so and exits.
- [rustup](https://rustup.rs) with the stable MSVC toolchain, and the Visual
  Studio Build Tools with the "Desktop development with C++" workload (the MSVC
  compiler and linker, and the Windows SDK, whose `rc.exe` embeds the
  manifest). ARM64 builds also need the "MSVC ARM64 build tools" component.

## Building on Windows

```powershell
cd windows
cargo test
cargo build --release    # target\release\audiosync.exe
```

For the other architecture, add its target first:

```powershell
rustup target add aarch64-pc-windows-msvc
cargo build --release --target aarch64-pc-windows-msvc
```

## Trying it on a PC

Turn on a speaker under This PC or Connected Speakers and streaming starts,
from All System Audio unless you pick an app under Audio Source. Windows can't
mute the original sound for AudioSync, so the app keeps playing on its own
output too, without the delay. To hear only AudioSync's copies, send the app
(Settings > System > Sound > Volume mixer) or the Windows default output to an
output you don't listen to, and turn the others on in AudioSync.
[docs/WINDOWS_AUDIO_ROUTING.md](../docs/WINDOWS_AUDIO_ROUTING.md) explains why
and lists the tests worth running on a real PC.

### AirPlay

AirPlay speakers that the sender can play are listed like any other speaker:
turn one on and it joins the stream. While an AirPlay speaker is on, the local
outputs wait for it: each plays at the sender's latency (1.75 s) plus its own
delay, counted up to 3000 ms, so they stay in step with the AirPlay speakers.
Turn the last AirPlay speaker off and each local output goes back to its own
delay. A speaker that stops answering stays on while AudioSync keeps trying
to reach it, with a notice.

Not supported yet: speakers that ask for a password, speakers that need
pairing with a PIN (such as an Apple TV that asks for one), and speakers that
only play AirPlay 2. They're listed with a note saying so, and AudioSync never
connects to them. Windows Firewall may ask whether AudioSync can use the
network the first time it looks for speakers; it needs private networks for
AirPlay.

If something acts up, start it from PowerShell with
`$env:AUDIOSYNC_DEBUG=1; .\audiosync.exe` and read
`%TEMP%\audiosync-debug.log`, which starts over at every launch and stops at
50 MB (`src/debug_log.rs`; [docs/DEVELOPMENT.md](../docs/DEVELOPMENT.md) says what it records).

## On a Mac

The Windows-only code sits behind `#[cfg(windows)]`, so `cargo test` runs the
logic tests on macOS too, and `cargo run` opens the window (without the Windows
version check or the tray icon, and with the icons of Segoe Fluent Icons drawn
as boxes), which is handy for UI work. There the window shows example speakers
from a mock backend: nothing plays and no audio device is opened. A Windows
build can be checked there but not linked:

```bash
rustup target add x86_64-pc-windows-msvc aarch64-pc-windows-msvc
cargo check --target x86_64-pc-windows-msvc
cargo check --target aarch64-pc-windows-msvc
```

## Where things are

- `src/main.rs`: startup
- `src/ui_model.rs`: what the windows show and can ask for (the `Backend`
  trait) and the mock backend the window runs on off Windows, unit-tested
- `src/adapter.rs`: the `Backend` on Windows. It turns what the window asks
  for into audio backend calls, with the session rules of `audiosync-core`
  (switching a speaker on starts streaming, a source that quits stops it), and
  keeps what the audio backend doesn't: the chosen source, groups,
  auto-connect marks and notices. Unit-tested over the audio backend's mock
- `src/view.rs`: fills the windows from the backend, turns user actions into
  backend calls, and polls the backend twice a second for what happens on its
  own (a device unplugged, a source that quit)
- `src/tray.rs`: the notification-area icon, its menu and the flyout's placement
  (`src/placement.rs`, unit-tested)
- `src/os_version.rs`: which Windows versions the app runs on (unit-tested)
- `src/platform.rs`: the Windows calls (`RtlGetVersion`, the message box, the
  taskbar theme, a monitor's work area, the time format of a notice)
- `src/audio/`: the audio backend the adapter drives. `wasapi/` is the
  Windows part (process loopback capture, one render stream per output,
  device notifications, what kind of device each output is); the rest is
  plain Rust over `audiosync-core` (which outputs play, at what delay and
  volume, what gets captured, the equalizer in the capture path) and is
  tested on any host with an in-memory mock driver. It hasn't run on real
  hardware yet.
- `ui/`: the Slint files, compiled by `build.rs`. `strings.slint` holds every
  user-facing string, `tokens.slint` (generated from `brand/tokens.json`) the
  colors and sizes
- `res/audiosync.exe.manifest`: supported Windows versions and per-monitor DPI
  awareness; `build.rs` embeds it with the icons and the version information
- `res/*.ico`, `res/audiosync-256.png`: the app and tray icons, generated from
  the brand mark by `scripts/brand/windows-icons.sh` (on a Mac)
- `deny.toml`: the licenses dependencies may use (`cargo deny check licenses`)

CI (`.github/workflows/windows.yml`) checks formatting, runs clippy and the
tests, builds both architectures in release mode and keeps each
`audiosync.exe` as a workflow artifact for a week, next to the
`THIRD-PARTY-NOTICES.txt` that `scripts/windows_notices.py` writes for that
build. It doesn't publish anything. To write the notices yourself (any OS,
for either target): `python3 -I scripts/windows_notices.py
x86_64-pc-windows-msvc`; the file lands next to `audiosync.exe` in
`windows/target/<target>/release/`.

## License

MIT, like the rest of AudioSync. Slint is used under its royalty-free license,
whose attribution is the `AboutSlint` widget in the About dialog, so keep it
there. See [THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md).
