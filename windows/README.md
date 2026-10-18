# AudioSync for Windows

This folder is the start of a Windows 11 version of AudioSync, written in Rust
with a [Slint](https://slint.dev) interface. The audio work will come from
`audiosync-core` in [`../core`](../core), the crate the macOS app links too.

**Status: in development and not usable yet.** It has the Mac app's screens
(the main window, the equalizer, a tray icon with a flyout, the About dialog),
filled with example speakers from a mock backend: nothing plays yet. It hasn't
been tried on real Windows hardware: CI builds and tests it, nothing more.
[docs/WINDOWS_UI.md](../docs/WINDOWS_UI.md) maps each part of the Mac main
window to its Windows counterpart and lists the keyboard shortcuts.

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

## On a Mac

The Windows-only code sits behind `#[cfg(windows)]`, so `cargo test` runs the
logic tests on macOS too, and `cargo run` opens the window (without the Windows
version check or the tray icon, and with the icons of Segoe Fluent Icons drawn
as boxes), which is handy for UI work. A Windows build can be checked there but
not linked:

```bash
rustup target add x86_64-pc-windows-msvc aarch64-pc-windows-msvc
cargo check --target x86_64-pc-windows-msvc
cargo check --target aarch64-pc-windows-msvc
```

## Where things are

- `src/main.rs`: startup
- `src/ui_model.rs`: what the windows show and can ask for (the `Backend`
  trait) and the mock backend, unit-tested
- `src/view.rs`: fills the windows from the backend and turns user actions into
  backend calls
- `src/tray.rs`: the notification-area icon, its menu and the flyout's placement
  (`src/placement.rs`, unit-tested)
- `src/os_version.rs`: which Windows versions the app runs on (unit-tested)
- `src/platform.rs`: the Windows calls (`RtlGetVersion`, the message box, the
  taskbar theme, a monitor's work area)
- `src/audio/`: the audio backend the UI will drive. `wasapi/` is the
  Windows part (process loopback capture, one render stream per output,
  device notifications); the rest is plain Rust over `audiosync-core`
  (which outputs play, at what delay and volume, what gets captured) and is
  tested on any host, along with a mock backend for UI work. It isn't wired
  to the window yet and hasn't run on real hardware.
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
`audiosync.exe` as a workflow artifact. It doesn't publish anything.

## License

MIT, like the rest of AudioSync. Slint is used under its royalty-free license,
whose attribution is the `AboutSlint` widget in the About dialog, so keep it
there. See [THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md).
