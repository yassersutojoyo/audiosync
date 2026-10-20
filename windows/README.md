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
been tried with a real speaker yet. Local playback has had a short first
listen on one Windows 11 PC (a USB-C in-ear and wired speakers on the jack,
playing together); Bluetooth outputs haven't been tried yet. CI builds and
tests it. [docs/WINDOWS_UI.md](../docs/WINDOWS_UI.md) maps each
part of the Mac main window to its Windows counterpart and lists the keyboard
shortcuts.

## Trying the preview exe

There's no Windows release yet. Every CI run of
`.github/workflows/windows.yml` keeps an `audiosync.exe` for x64 and one for
ARM64 as workflow artifacts for a week (you need to be signed in to GitHub to
download them), or you can build it yourself (below). It's a single exe with
no installer: put it in a folder of its own, keep `THIRD-PARTY-NOTICES.txt`
from the same download next to it, and run it.

- **What you need:** Windows 11 (build 22000 or later) on x64 or ARM64. On
  anything older the app says so and exits.
- **SmartScreen:** the exe isn't code-signed, so the first time Windows may
  say it protected your PC. Click More info, then Run anyway. That's expected
  for this preview.
- **Firewall:** the first time AudioSync looks for AirPlay speakers, Windows
  Firewall may ask whether it can use the network. Allow it on private
  networks. If your Wi-Fi or Ethernet is set to Public, Windows will probably
  block the search and no AirPlay speakers show up; switching the network to
  Private usually fixes it
  (Settings > Network & internet > your connection > Network profile type).
  You can also allow AudioSync on Public networks in Windows Security >
  Firewall & network protection > Allow an app through firewall. If no
  AirPlay speaker turns up within 20 seconds, the speaker list says so.
- **One copy at a time:** starting it again while it runs brings the running
  copy's window to the front instead of opening a second one.
- **Closing and quitting:** closing the window keeps AudioSync running in the
  notification area, next to the clock. To quit, right-click the tray icon and
  choose Exit AudioSync, or use File > Exit in the main window.
- **Where your stuff is:** your choices (source, speakers with their delays
  and volumes, master volume, groups, auto-connect, the equalizer, where the
  window was) are saved in `%LOCALAPPDATA%\AudioSync\settings.json`, next to
  `airplay-pairings.dat`, the Apple TV pairings, encrypted for your Windows
  account. To start over, quit AudioSync and delete that folder. A launch
  doesn't start streaming by itself unless an auto-connect speaker is there,
  and a speaker that's missing keeps its settings for when it's back.
- **Debug log:** if something acts up, quit AudioSync from the tray, then
  start it from PowerShell in the exe's folder with
  `$env:AUDIOSYNC_DEBUG=1; .\audiosync.exe`. The log goes to
  `%TEMP%\audiosync-debug.log`; it starts over at every launch and stops at
  50 MB. [docs/DEVELOPMENT.md](../docs/DEVELOPMENT.md) says what it records.
  Attach it when you report a problem.

## What you need to build it

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

A speaker that sounds quiet even with AudioSync's slider at 100 is probably
turned down in Windows itself, since AudioSync plays through that volume too.
When a speaker that's on is at 25 % or less, or muted, in Windows, its row says
so under the slider ("Windows volume is 13%", "Muted in Windows"), and one click
on Set to 75% or Unmute fixes it. AudioSync never touches that volume unless
you click.

If another app holds a speaker for itself (exclusive mode), that speaker
stays on with a notice, and AudioSync plays to it as soon as it's free. If
Windows restarts its audio service while you stream, AudioSync picks the
capture up again by itself after a few seconds.

### AirPlay

AirPlay speakers that the sender can play are listed like any other speaker:
turn one on and it joins the stream. While an AirPlay speaker is on, the local
outputs wait for it: the PC's own speakers play after the AirPlay delay (the
sender's latency, 1.75 s, plus whatever you set under AirPlay Delay), and
every other output by its own delay on top of that, so they stay in step with
the AirPlay speakers. Turn the last AirPlay speaker off and the local outputs
only line up with each other again. A speaker that stops answering stays on
while AudioSync keeps trying to reach it, with a notice.

Delays are relative to the PC's own speakers (or line out), which have no
delay field: give a speaker that sounds late, like a Bluetooth one, a
negative delay (around −150 to −300 ms), and the others wait for it. Settings
saved by an older version are converted once, so everything sounds as it did.

An Apple TV that asks for a code is paired the first time you switch it on:
type the 4-digit code it shows, and AudioSync remembers the pairing; Forget
Pairing in the speaker's menu undoes it.

If AirPlay stops after an internal error, or can't start (at launch too), a
notice says "AirPlay stopped or couldn't start" with a Retry button that
starts it again.

Not supported yet: speakers that ask for a password, speakers that need
another kind of pairing (the Apple TV code is the one AudioSync does),
and speakers that only play AirPlay 2. They're listed with a note saying so,
and AudioSync never connects to them.

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
- `src/single_instance.rs`: one copy per Windows session; a second launch
  asks the running copy to show its main window, then exits (unit-tested)
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
  tested on any host with an in-memory mock driver. On real hardware it has
  had a short first listen on one Windows 11 PC; Bluetooth not tried yet.
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

Copyright (C) 2026 Yasser Sutojoyo Kusumo. GPL-3.0-or-later, like the rest of
AudioSync. Slint offers its crates under GPL-3.0-only or its own royalty-free
and commercial licenses; public builds use them under GPL-3.0-only, so a
built `audiosync.exe` as a whole can be passed on under GPL-3.0 only. The About
dialog still shows Slint's `AboutSlint` widget as a credit. See
[THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md).
