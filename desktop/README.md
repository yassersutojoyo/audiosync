# AudioSync for Windows

This folder is the start of a Windows 11 version of AudioSync, written in Rust
with a [Slint](https://slint.dev) interface. The audio work comes from
`audiosync-core` in [`../core`](../core), the crate the macOS app links too.

**Status: in development, ready for a first try on a real PC.** It has the Mac
app's screens (the main window, the equalizer, a tray icon with a flyout, the
About dialog), and on Windows they drive the real audio backend: it captures
one app, everything the PC plays except AudioSync itself, or an input device
such as a microphone or line in, and plays it on
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

Since 0.7.1 the zips are on the
[Releases](https://github.com/yassersutojoyo/audiosync/releases) page, or build
it yourself (below). It's a single exe with no installer: put it in a folder of
its own, keep the `THIRD-PARTY-NOTICES.txt` that comes with it next to it (for a
build you make yourself, you write that file as shown below), and run it.

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
- **Renaming a speaker:** double-click its name, or right-click it or use its
  … button and choose Rename… from its menu. Leave the name empty to get the
  device's own name back.
- **One copy at a time:** starting it again while it runs brings the running
  copy's window to the front instead of opening a second one.
- **Closing and quitting:** closing the window keeps AudioSync running in the
  notification area, next to the clock. To quit, right-click the tray icon and
  choose Exit AudioSync, or use File > Exit in the main window.
- **Where your stuff is:** your choices (source, speakers with their delays
  and volumes, master volume, groups, auto-connect, the equalizer, where the
  window was) are saved in `%LOCALAPPDATA%\AudioSync\settings.json`. Next to
  it sit `airplay-pairings.dat`, the Apple TV pairings, encrypted for your
  Windows account; `cast-speakers.json`, each Google Cast speaker's offset;
  and `remote-devices.json`, the phones paired with Web Remote.
  To start over, quit AudioSync and delete that folder. A launch doesn't
  start streaming by itself unless an auto-connect speaker is there,
  and a speaker that's missing keeps its settings for when it's back.
- **Google Cast:** Cast speakers show up under Connected Speakers next to
  the AirPlay ones, and one plays at a time. Cast has no shared clock, so you
  set each one's offset by ear in its row; if it drifts, Resync under its
  volume restarts it with a short gap. A group made in the Google Home app
  plays as one speaker. If one doesn't show up, Speakers > Add by Address…
  adds it by its IP address or host name.
- **Web Remote:** File > Web Remote… lets a phone on the same network switch
  your speakers on and off and set volumes and delays. Turn it on, open the
  address it shows on the phone (or scan the QR code), add the page to the
  Home Screen, then open it from there and enter the pairing code. It's plain
  HTTP, so I'd keep it to your home network. Forget All Remote Devices
  unpairs every phone.
- **Calibrate with Microphone:** under a local speaker, this sets its delay
  for you. It plays a short sweep on the PC's own speakers and then on that
  one, and listens with the default microphone, so streaming and that speaker
  need to be on. If Windows blocks the microphone for desktop apps, AudioSync
  says so and offers the privacy settings.
- **Calibrate All Speakers:** the button in the Calibration card of the main
  window does the same for every speaker that's on, one at a time, then lists
  the results. Apply sets them all, Undo puts them all back, and Measure Again
  repeats one speaker.
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
cd desktop
cargo test
cargo build --release    # target\release\audiosync.exe
```

Then, from the repository root, write the notices for that build next to it:

```powershell
py -3 -I scripts/windows_notices.py x86_64-pc-windows-msvc --output desktop/target/release/THIRD-PARTY-NOTICES.txt
```

For the other architecture, add its target first:

```powershell
rustup target add aarch64-pc-windows-msvc
cargo build --release --target aarch64-pc-windows-msvc
```

## Trying it on a PC

Turn on a speaker under This PC or Connected Speakers and streaming starts,
from All System Audio unless you pick an app or an input device under Audio
Source. Windows can't
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

A speaker protected by a password asks for it when you switch it on.
AudioSync keeps it in memory until you quit, so it asks again on the next
launch, as on the Mac. A wrong password gets you the prompt again.

If AirPlay stops after an internal error, or can't start (at launch too), a
notice says "AirPlay stopped or couldn't start" with a Retry button that
starts it again.

Not supported yet: speakers that need another kind of pairing (the Apple
TV code is the one AudioSync does). They're listed with a note saying so,
and AudioSync never connects to them. Speakers that only play AirPlay 2
play with nothing to turn on. A speaker that does both AirPlay 1 and AirPlay 2
still uses AirPlay 1 unless you turn on File > AirPlay 2 (Experimental), then
quit and reopen AudioSync.

### Google Cast

Google Cast speakers show up under Connected Speakers, one playing at a
time. Cast has no shared clock, so AudioSync can't sync them by itself: each
one has an offset you set by ear in its row. Hear it late? Raise the offset
and the other speakers wait longer. AudioSync remembers it per speaker. If
one drifts, Resync under its volume restarts the stream with a short gap.
Its slider sets the speaker's own volume, and Master Volume scales it like
every other speaker.

A group made in the Google Home app plays as one speaker, with one volume
and one offset, and its row names the members once the group reports them.
The speakers in it keep their own rows. AirPlay speakers wait for a Cast
speaker that plays later than them, but only up to 20 s; past that a notice
says how far ahead they play.

If none show up, check that the PC is on the same network and that the
network is set to Private, or that Windows Firewall allows AudioSync on it.
If one still doesn't, choose Speakers > Add by Address…, or the Add by
Address… link in the speaker list, and enter its IP address or host name.
AudioSync keeps the address, and Remove in the speaker's menu forgets it.

### Web Remote

File > Web Remote… turns on a page that a phone on the same network can
open, by its address or the QR code in the window. Add the page to the Home
Screen first, then open it from there and enter the pairing code: on some
phones a pairing made in the browser doesn't carry over. Each code works
once and for 10 minutes, and New Code makes a fresh one. The phone can then
switch speakers on and off, set their volume, their delay and Master
Volume, and pick the audio source from the same list as the main window.
The page is plain HTTP, so keep it to your home network. Forget All
Remote Devices unpairs every phone. If another app already uses the port,
the window says so and lets you pick another one.

### Calibrate with Microphone

A local speaker that has a delay field gets a Calibrate with Microphone
button under its delay. Put the microphone where you listen and click it:
AudioSync fades the music out, plays a short, quiet sweep on the PC's own
speakers and then on that speaker, and sets the delay from the difference.
Windows has to let desktop apps use the microphone (Settings > Privacy &
security > Microphone), and a Bluetooth headset's hands-free mic won't do,
since recording from it switches the headset to call mode.

### Calibrate All Speakers

The Calibration card in the main window measures every speaker that's on
against the PC's own speakers, AirPlay and Google Cast included: it fades
the music out, plays a short sweep on each one in turn, twice, and fades
the music back in. A Cast speaker without a stored offset takes longer to
find. You get a list of results, and nothing changes until you choose
Apply. Undo puts the old delays back, and Measure Again redoes one speaker.
Headphones and virtual outputs are left out, since the microphone can't
hear them; Include Anyway on such a row measures it with the others until
the results close.

## On a Mac

The Windows-only code sits behind `#[cfg(windows)]`, mostly in `src/os/`, so
`cargo test` runs the logic tests on macOS too, and
`cargo run --features mock-backend` opens the window (without the Windows
version check or the tray icon, and with the app's own icon drawings in place
of Segoe Fluent Icons, as on Linux), which is handy for UI work. There the
window shows example speakers from a mock backend: nothing plays and no audio
device is opened (a plain `cargo run` there says it can't start). Release
builds never turn that feature on. A Windows build can be checked there but
not linked:

```bash
rustup target add x86_64-pc-windows-msvc aarch64-pc-windows-msvc
cargo check --target x86_64-pc-windows-msvc
cargo check --target aarch64-pc-windows-msvc
```

## Where things are

- `src/main.rs`: startup
- `src/os/`: the per-system code behind one interface: `windows.rs` (the
  WASAPI backend and media sessions, single instance, tray icon,
  `%LOCALAPPDATA%`, DPAPI, the PC's name and the time format), `other.rs`
  (the UI-only build) and `engine.rs`, which starts the AirPlay sender, the
  Google Cast host and the adapter over any audio driver
- `src/single_instance.rs`: one copy per Windows session; a second launch
  asks the running copy to show its main window, then exits (unit-tested)
- `src/ui_model.rs`: what the windows show and can ask for (the `Backend`
  trait) and the mock backend, built only with the `mock-backend` feature
  (off Windows it runs the window, on Windows it backs the
  `AUDIOSYNC_PREVIEW_ZONES` and `AUDIOSYNC_PREVIEW_PRESETS` previews),
  unit-tested
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
  Windows part (process loopback and input device capture, one render
  stream per output,
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
`desktop/target/<target>/release/`.

## License

Copyright (c) 2026 Yasser Sutojoyo Kusumo. GPL-3.0-or-later, like the rest of
AudioSync. Slint offers its crates under GPL-3.0-only or its own royalty-free
and commercial licenses; public builds use them under GPL-3.0-only, so a
built `audiosync.exe` as a whole can be passed on under GPL-3.0 only. The About
dialog still shows Slint's `AboutSlint` widget as a credit. See
[THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md).
