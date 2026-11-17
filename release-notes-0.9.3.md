In 0.9.3 the Windows and Linux app catches up with the Mac's: the same layout, the same menus and sheets, the same motion. All three also get a few new things together.

### On every system

- Resume Playback After Sleep, a new switch in Settings, off by default. Turn it on and the song that was playing when your computer went to sleep plays again once your speakers are back after the wake.
- Speaker rows show AudioSync's own device icons.
- Next and Previous slide the new cover in from the side while the old one shrinks away, and Play/Pause turns with a quick shrink and grow. Reduce Motion, or animations turned off in Windows or Linux, keeps it to a cross-fade.
- The Presets row folds open into a list with each preset's options on its row, and the preset and curve you picked show under Presets and Equalizer.

### macOS

- The built-in speakers' row shows the macOS volume and opens a slider for it, and each local output gets back the volume it last played at through AudioSync.
- A Hide Dock Icon When Window Closes switch in Settings, off by default.
- Delete Zone asks first.
- Fixed: Caps Lock no longer quits AudioSync on macOS 27 while it controls the volume keys, Now Playing reads Spotify 1.3.9, a Bluetooth speaker that drops and comes back plays in time again, and the Settings window opens at its full height.

### Windows

- An installer, `AudioSync-Setup.exe` for x64 and `AudioSync-Setup-arm64.exe` for ARM64, next to the zips. It installs for everyone on the PC by default, or only for you. An install for everyone updates from Download and Install after one administrator prompt per update; if you decline the prompt, nothing changes.
- The main window matches the Mac's: a gear menu in place of the menu row, a Settings window, menus in the app's light or dark theme, Calibrate All and Add by Address as sheets over the window, the Mac's notices, and text in Segoe UI Variable.
- With All System Audio, the PC's default output no longer plays the sound a second time, and with one app as the source, only that app is moved off its output while it streams.
- The This PC row shows the Windows volume, and each local output gets back the level it last played at.
- A new app icon: the white AudioSync mark on a teal tile.

### Linux

- An `.rpm` for Fedora on x86_64, next to the AppImages and `.deb` files.
- The main window matches the Mac's too, with the same gear menu, Settings window, menus and sheets as on Windows, and text in Inter.

Everything else is in the [changelog](https://github.com/yassersutojoyo/audiosync/blob/main/CHANGELOG.md).

### Install

macOS 15 or later, Apple silicon. From Terminal: `curl -fsSL https://github.com/yassersutojoyo/audiosync/releases/latest/download/install.sh | bash`. It downloads the release zip, checks it against `SHA256SUMS.txt`, puts AudioSync in Applications (so it needs an administrator account) and opens it, with no Open Anyway step. Or open `AudioSync.dmg` and drag the app to Applications; on the first launch, click Done, then System Settings → Privacy & Security → Open Anyway.

Windows 11, x64 or ARM64. From PowerShell, no admin: `irm https://github.com/yassersutojoyo/audiosync/releases/latest/download/install.ps1 | iex`. Or the installer, or a zip unzipped into its own folder. The exe and the installer are unsigned, so SmartScreen warns on the first launch: click More info, then Run anyway. AirPlay on Windows reaches receivers that take AirPlay 1, with a password prompt for a receiver that has one and a code prompt for an Apple TV that asks for one, and AirPlay 2: AirPlay 2-only speakers, stereo pairs and home theater groups play by default, and a speaker that does both uses AirPlay 2, or AirPlay 1 when it was paired over AirPlay 1 or AirPlay 2 can't take it.

Linux, x86_64 or aarch64, glibc 2.39 or later, PipeWire. From a terminal, no sudo: `curl -fsSL https://github.com/yassersutojoyo/audiosync/releases/latest/download/install-linux.sh | bash`. Or the AppImage, the `.deb` or the `.rpm`. The AppImage updates itself; a `.deb` or `.rpm` install updates by hand from this page.
