# Platform differences

AudioSync is meant to behave the same on macOS, Windows and Linux. This page lists the places where it can't, because the system it runs on doesn't offer what the other two do or has its own rules. If you find a difference that isn't here, it's a bug, so please [open an issue](https://github.com/yassersutojoyo/audiosync/issues).

## Hearing the source twice

- **Mac:** the app or system sound AudioSync captures is muted where it used to play, so you only hear AudioSync's delayed copies. macOS can capture and mute an app's sound in one step.
- **Windows:** Windows can't mute an app without also silencing the capture. So while AudioSync streams the whole system, it makes a virtual output (a virtual cable, for example) Windows' default and puts your default back when it stops. With one app as the source, it moves only that app. With no virtual output on the PC, your default output simply gets no copy from AudioSync, since it already plays the original. Windows has no way for an app to create an output without installing a driver, so the x64 installer adds VB-CABLE by VB-Audio as that output, and Fix It on the double-audio notice adds it on a PC the installer didn't reach. The Mac and Linux need no driver for this. If the PC loses power mid-stream, Windows keeps the virtual output as the default, so while AudioSync has moved something it also adds a startup entry that puts your output back at your next sign-in and then removes itself.
- **Linux:** AudioSync adds its own silent output to PipeWire while it runs and makes it the default while the whole system streams, so no virtual output is needed. That output goes away with AudioSync, so after a power cut PipeWire plays on a real output again without any startup entry.

## Calls while the whole system streams

An app that's using a microphone, like a call, is left out of a whole-system stream, so you hear the call right away on your own output instead of late on the network speakers. It joins the stream again a few seconds after it stops using the microphone.

- **Mac:** the app is left out of what AudioSync captures, so macOS plays it on your output as usual. No app can hold on to a Mac output the way the Windows case below describes, so there's nothing like it here.
- **Windows:** the app's own output is set to the output that was your default before AudioSync switched it. A browser moves with it: Edge on Windows 11 moved a sound it was already playing in under a tenth of a second, and Chrome on a test server played a call's sound on your own output when the call was joined the usual way, microphone first. An app that keeps playing on one output it picked for itself can't be moved this way, and its call stays in the stream. Windows' communications output is never the one AudioSync switches to. Zoom and Teams as desktop apps haven't been tested, so how they behave here is unknown.
- **Linux:** the app's playback is pointed at the output that was your default before AudioSync's own output took over. PipeWire has no way for an app to hold on to AudioSync's output against that, so there's nothing like the Windows case above.

## Now Playing

- **Mac:** the Now Playing card shows Spotify and Music only, and macOS asks once whether AudioSync may control each of them. macOS doesn't let other apps read its system-wide Now Playing info, so AudioSync talks to those two players directly.
- **Windows and Linux:** any player that reports what it plays shows up (Windows' media sessions, MPRIS on Linux), with no permission prompt.

## Where the app lives

- **Mac:** a menu bar item plus a main window. About, Check for Updates and the Guide sit in the app's menus in the macOS menu bar, since every Mac app has them there. Hide Dock Icon When Window Closes is a Mac-only setting, because only macOS has a Dock.
- **Windows and Linux:** a window plus a tray icon. Those menu items sit in the gear menu in the window, since there's no app menu bar at the top of the screen.
- **Linux:** the tray icon needs a desktop that shows StatusNotifier icons. Without one, AudioSync runs as a window only: closing the window quits, and a launch at sign-in opens the window. Clicking the icon opens the tray panel by the pointer, since the desktop doesn't say where the icon is, and on Wayland the desktop places the panel itself. A desktop that only shows the icon's menu keeps Open AudioSync in it.
- **Linux on Wayland:** opening AudioSync while it already runs brings its window up, but maybe without the focus. Wayland only hands the focus over with a token the toolkit can't pass on yet.

## Volume keys

Volume Keys Control AudioSync: Mac and Windows only. On Linux the desktop keeps the volume keys, so they change the desktop's volume and AudioSync's master stays where you set it. On Mac and Windows the switch takes the keys away from the system as soon as it's on, and Linux has no way to do that:

- **Wayland:** an app only gets keys while its window has the focus. The one route to a key anywhere on the desktop is the [Global Shortcuts portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.GlobalShortcuts.html), where the app can only suggest a key and you pick it in the desktop's own dialog. The desktop's volume control already holds the volume keys, so you'd have to take them away from it by hand first. GNOME has the portal only [since GNOME 48](https://release.gnome.org/48/developers/), so GNOME 46 and 47 have no route at all.
- **X11:** an app takes a key for the whole screen with `XGrabKey`, and that [fails with BadAccess](https://tronche.com/gui/x/xlib/input/XGrabKey.html) when another program holds the same key. GNOME and KDE hold the volume keys themselves. Watching the keys without taking them would move both volumes at once.

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

## A graphics driver reset

- **Mac:** not applicable. The Mac app's windows are AppKit's and don't draw through the renderer the other two use.
- **Windows and Linux:** when a driver reset takes the window's surface away, AudioSync starts itself again and the speakers come back. If that happens three times in ten minutes, Windows switches to drawing in software and carries on, while Linux, which has no software fallback in the app, stops with a message.

## Updates

- **Mac and Windows:** Install Update replaces the app. On Windows, an install for everyone on the PC asks for an administrator once per update, as Windows requires for files in Program Files.
- **Linux:** the AppImage updates itself. The `.deb` and `.rpm` belong to your package manager, so you update them by installing the new package.

## A speaker's status popover

Every speaker row shows a short status, and the (i) next to it opens the rest. The words, spacing and keys are the same everywhere; only the popover's frame differs.

- **Mac:** a standard macOS popover, with an arrow pointing at the (i).
- **Windows and Linux:** a Windows 11 style flyout with no arrow, since those flyouts don't have one.

## The bad-network test switch

`AUDIOSYNC_BAD_NETWORK` makes AirPlay audio lose and delay some of its own packets, to reproduce a bad Wi-Fi day in a test run ([`DEVELOPMENT.md`](DEVELOPMENT.md)). It does the same thing everywhere; only the launches that read it differ.

- **Windows and Linux:** only a named test copy (`AUDIOSYNC_INSTANCE`) with its debug log on.
- **Mac:** there's no test copy, so any launch with the debug log on (`AUDIOSYNC_DEBUG=1`).
