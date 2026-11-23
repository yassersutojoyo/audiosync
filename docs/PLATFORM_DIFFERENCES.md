# Platform differences

AudioSync is meant to behave the same on macOS, Windows and Linux. This page lists the places where it can't, because the system it runs on doesn't offer what the other two do or has its own rules. If you find a difference that isn't here, it's a bug, so please [open an issue](https://github.com/yassersutojoyo/audiosync/issues).

## Hearing the source twice

- **Mac:** the app or system sound AudioSync captures is muted where it used to play, so you only hear AudioSync's delayed copies. macOS can capture and mute an app's sound in one step.
- **Windows:** Windows can't mute an app without also silencing the capture. So while AudioSync streams the whole system, it makes a virtual output (a virtual cable, for example) Windows' default and puts your default back when it stops. With one app as the source, it moves only that app. With no virtual output on the PC, your default output simply gets no copy from AudioSync, since it already plays the original. Windows has no way for an app to create an output without installing a driver. If the PC loses power mid-stream, Windows keeps the virtual output as the default, so while AudioSync has moved something it also adds a startup entry that puts your output back at your next sign-in and then removes itself.
- **Linux:** AudioSync adds its own silent output to PipeWire while it runs and makes it the default while the whole system streams, so no virtual output is needed. That output goes away with AudioSync, so after a power cut PipeWire plays on a real output again without any startup entry.

## Now Playing

- **Mac:** the Now Playing card shows Spotify and Music only, and macOS asks once whether AudioSync may control each of them. macOS doesn't let other apps read its system-wide Now Playing info, so AudioSync talks to those two players directly.
- **Windows and Linux:** any player that reports what it plays shows up (Windows' media sessions, MPRIS on Linux), with no permission prompt.

## Where the app lives

- **Mac:** a menu bar item plus a main window. About, Check for Updates and the Guide sit in the app's menus in the macOS menu bar, since every Mac app has them there. Hide Dock Icon When Window Closes is a Mac-only setting, because only macOS has a Dock.
- **Windows and Linux:** a window plus a tray icon. Those menu items sit in the gear menu in the window, since there's no app menu bar at the top of the screen.
- **Linux:** the tray icon needs a desktop that shows StatusNotifier icons. Without one, AudioSync runs as a window only: closing the window quits, and a launch at sign-in opens the window. Clicking the icon opens the tray panel by the pointer, since the desktop doesn't say where the icon is, and on Wayland the desktop places the panel itself. A desktop that only shows the icon's menu keeps Open AudioSync in it.
- **Linux on Wayland:** opening AudioSync while it already runs brings its window up, but maybe without the focus. Wayland only hands the focus over with a token the toolkit can't pass on yet.

## Volume keys

Volume Keys Control AudioSync: Mac and Windows only. On Linux the desktop keeps the volume keys (Wayland lets no app grab keys for the whole desktop), so they change the desktop's volume and AudioSync's master stays where you set it.

## Permissions and the network

- **Mac:** macOS asks once for Local Network access (to find speakers) and for permission to record system audio, and for the microphone when you pick an input device or calibrate.
- **Windows:** when no speaker is found and the PC is on a network Windows marks Public, the note suggests allowing AudioSync in Windows Firewall or setting the network to Private. Only Windows tells apps a network's profile.

## Motion

AudioSync's animations follow the system's own switch, which has a different name on each: Reduce Motion on the Mac, Animation effects on Windows, and GNOME's animations setting on Linux. Linux reads GNOME's setting; on other desktops the animations stay on.

## Screen readers

- **Mac:** a notice that is closing drops out of VoiceOver at once and takes no clicks (`Sources/AudioSync/NoticeMotion.swift:51-52`).
- **Windows and Linux:** a closing notice takes no clicks, but Narrator or Orca can still reach it during its fade, about a third of a second. The toolkit the app is built with can't hide a control from a screen reader while it is still drawn.

## Text

The app's text uses the system font on the Mac and Segoe UI Variable on Windows. Linux has no font every distribution ships, so AudioSync brings Inter along and looks the same everywhere.

## Updates

- **Mac and Windows:** Install Update replaces the app. On Windows, an install for everyone on the PC asks for an administrator once per update, as Windows requires for files in Program Files.
- **Linux:** the AppImage updates itself. The `.deb` and `.rpm` belong to your package manager, so you update them by installing the new package.
