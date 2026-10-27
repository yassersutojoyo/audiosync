# The Windows app's screens

The Windows app (`desktop/`) has the same screens as the Mac app, in the same
order, with Windows manners: a menu bar with Ctrl shortcuts, a notification-area
(tray) icon with a flyout where the Mac has its menu bar panel, the system's
window controls, and light or dark as Windows is set unless View > Appearance
says otherwise. The wording follows the
Mac app's `UIStrings`; all of it lives in `desktop/ui/strings.slint`.

Colors, the wordmark typeface, font sizes and the window margins come from the
shared design tokens (`brand/tokens.json`, see `brand/README.md`). The rest
follows Windows 11 and lives only in the Windows app (the `Fluent` values in
`desktop/ui/widgets.slint`): text in Segoe UI Variable, 8 px corners on cards,
notices and the flyout and 4 px on buttons and rows, 1 px card edges, hover and
pressed fills, and short state changes (83 ms). Every titled window (main,
equalizer, both guides, Web Remote, About, the zone dialog, the Apple TV code and password dialogs and the Cast address dialog) asks Windows for Mica in its
title bar (Windows 11 22H2 and later), on content in the token background,
which is close to Mica's own tint; the tray flyout gets the system's rounded frame, border and shadow.
Where Windows doesn't offer one, the window keeps the plain frame (and the
flyout its own drawn edge). The dialogs (About, the zone dialog and the
Apple TV code dialog) have only a close button in the title bar, as Windows
dialogs do. The standard controls (switches, sliders, drop-downs, the
primary button) take their accent from the brand teal instead of the Windows
accent color, so the app has one accent: the Fluent style keeps its own
lightness and takes the teal's hue, dark teal in light mode and mint in dark
mode. AudioSync's own pieces use the brand teal tokens. Text fields are
AudioSync's own: at the style's lightness, selected text in teal measured
4.21:1, so a field selects in the teal text color with the surface color on
it (5.85:1 in light mode, 5.6:1 in dark), and a teal line under it shows the focus.
Text longer than the field slides left so the cursor stays in view, and a
right click offers Undo, Redo, Cut, Copy, Paste and Select All.

The About dialog (Help > About AudioSync) shows the brand mark and the
wordmark on top, then the version, the audio core's version, the copyright and
the license, and the Slint badge small at the bottom.

The main window has a header (mark, wordmark, status and the Streaming switch),
then a left column of cards (Audio Source, Now Playing while a track shows,
Master Volume, Equalizer, Calibrate All Speakers) and the
speaker list. A speaker row has the toggle, then, as on the Mac, a line with
the name, the kind below it (and the availability note, when there is one), the
delay and the "…" options button, and the volume slider with its value under
that line, so a long name isn't cut short by the slider. The delay lines up
across the sections. The window can't be made narrower than 860 px or shorter
than 440 px, which fits a 1366 x 768 screen at 150 %, and it holds that size
when another program resizes it. Both columns scroll when the window is too
short for them, and the speaker list keeps room for its scroll bar.

A speaker row shows one of four states. Off: a grey toggle. On while
streaming is off: an outlined toggle, "Will play" to Narrator. Playing: a
filled toggle, a teal row and a bold name. On while streaming but unable to
play right now: an outlined toggle, a plain row and name, and a note under the
name that Narrator also reads as the toggle's value, "Waiting for device" when
another app holds the output's device for itself, or "Reconnecting…" while
AudioSync keeps trying to reach an AirPlay speaker. That speaker stays on, so
selecting it turns it off; the row goes back to playing as soon as it plays
again, and the notice explains what happened. The tray flyout's row shows the
same note under the name. A zone header waits too, with the same note, when
every present member waits (it shows the first present member's note).

The windows read their data through the `Backend` trait in
`desktop/src/ui_model.rs`. On Windows that is the adapter over the WASAPI audio
backend (`desktop/src/adapter.rs`), which captures and plays for real. On other
hosts, where the UI is worked on, a mock with example speakers stands in:
nothing plays and no audio device is opened there.

Settings are kept across launches in `%LOCALAPPDATA%\AudioSync\settings.json`
(`desktop/src/settings.rs`): the source (an input device too, and each input device's channel pair), each speaker's switch, delay and
volume, the AirPlay delay, the master volume, auto-connect marks, the names given to speakers, the zones (name, members and volume), the equalizer, View >
Show Virtual Devices, View > Appearance, File > Switch to All System Audio When the Source App Quits,
File > AirPlay 2 (Experimental), the web remote's switch and port, the Cast
speakers added by address, and the main window's size and place (fitted to the
monitor it opens on) and whether it was maximized. A file from a newer
AudioSync is ignored and kept as `settings.newer-<UTC time>.json` before the
first save over it. One that's damaged, or that can't be read at launch even
at a second try half a second later (another program may hold it for a
moment), is ignored too and kept as `settings.ignored-<UTC time>.json`: right
at launch when it can be read then, otherwise as soon as it can (tried every
few seconds, and before the first save over it). A notice then says the
settings couldn't be read at launch, that AudioSync started with the
defaults, and names that copy, even if nothing is changed. The Apple TV
pairings' file works the same way.
Each copy is a new file, so a later one never replaces an earlier one. While
the file still can't be read, nothing is saved over it and the "couldn't save
your settings" notice shows. As on the Mac, a launch starts streaming only
through an auto-connect speaker that is there.
Apple TV pairings have their own file (see the pairing prompt below), kept the
same way: one that can't be read at launch is kept as
`airplay-pairings.ignored-<UTC time>.dat` before the next pairing writes a new
one, with a notice that names it.

## Mac main window to Windows

| Mac main window | Windows | Notes |
|---|---|---|
| Header: mark, wordmark, status line | Same | The mark is grey while streaming is off, as on the Mac |
| Streaming switch | Switch in the header; Speakers > Turn Streaming On/Off; the tray menu | |
| Options (gear) menu: New Zone from Active Speakers…, Refresh Devices, Hidden Speakers | Speakers menu: New Zone from Active Speakers… (Ctrl+N), Refresh Devices (F5), Hidden Speakers | Windows keeps commands in the menu bar. Hidden Speakers lists each hidden speaker as "name (kind)", or "name (kind, not found)" while it isn't there, sorted by name; choosing one lists it again, off. With none hidden it holds "No hidden speakers", disabled |
| Options menu: Appearance (and the app menu's) | View > Appearance: Match System, Light, Dark | Match System by default, and remembered. The choice applies at once to every window, the tray flyout, the title bars and the main window's menu bar; Match System follows Windows when its mode changes. The tray icon and its right-click menu keep following Windows. A value in the settings file this build doesn't know reads as Match System |
| Options menu: Show Virtual Devices | View > Show Virtual Devices | Off by default and remembered, as on the Mac: a virtual output (a loopback driver, a voice changer) is listed only while this is on or while the output is turned on. Once listed it stays listed for the session (until the app restarts or this setting changes), so turning it off doesn't pull its row away |
| Notices | Same, each with a dismiss button, plus Dismiss All | The tray flyout shows the newest one and "n more notices…". Windows posts them for streaming turned on with no speaker, an app that quit (so streaming stopped, or, with File > Switch to All System Audio When the Source App Quits on, went on with All System Audio) or isn't running, an input device that isn't connected, was disconnected or is blocked by the microphone privacy settings, capture that failed (when Windows audio restarts mid-stream, AudioSync first tries to capture again by itself for a few seconds, and the notice comes only if that doesn't work or the capture is lost again right away), a speaker that couldn't start, a speaker that was disconnected (that one clears itself once the speaker is back, also when a USB speaker comes back in another USB port, where it keeps its settings; a speaker whose stream only restarts, after a format change or a wake, gets none), a speaker another app holds for itself in exclusive mode (it stays on and AudioSync tries it again every few seconds; the notice says to close that app or stop apps from taking exclusive control of the speaker in Sound settings, and clears itself once the speaker plays, streaming stops or the speaker is unplugged), AirPlay that stopped after an internal error or couldn't start, or couldn't start looking for speakers, at launch too (its Retry starts it again), an AirPlay speaker that's on while Windows has no working sound output, so apps have nowhere to play (it clears itself once an output is back or no AirPlay speaker is on), settings or Apple TV pairings that couldn't be saved, so changes made now won't be kept after AudioSync quits (told once while saving keeps failing, cleared once a save works again), settings or Apple TV pairings that couldn't be read at launch, so AudioSync started without them, once the old file has been kept aside (told once, with the copy's name), and a source you may hear twice because it also plays straight from an output. That last one names the outputs you can mute apart from the ones AudioSync plays to as well, clears itself once the cause is gone, and once dismissed stays away for the same source and outputs ([how it's detected](WINDOWS_AUDIO_ROUTING.md#detection)). Its Show How button opens a short guide that fits the case: for one app, send it to an output you don't listen to in the Volume mixer (for all apps except some, each other app); for the whole system, make such an output the Windows default (and move any app that plays straight to another output). Muting is offered only when every output named is one AudioSync doesn't play to, and the guide warns that muting the app itself silences AudioSync too. Its buttons open the Volume mixer or Sound settings; AudioSync never changes those settings itself |
| Audio Source | Same (a drop-down list) | All System Audio (everything but AudioSync), then each app that plays audio, by name, then each connected input device (a microphone, line in), by name. A Bluetooth headset's hands-free microphone isn't offered, since opening it switches the headset to call quality. The list is read again every few seconds and with Refresh Devices; the chosen app stays chosen while it's paused or after it quits, and streaming finds it whenever it runs (restarted too), even before it plays again. A chosen input device stays chosen while it's unplugged: streaming stops with a notice when it's pulled out, and starts again only once it's back. Windows' microphone privacy settings can block desktop apps; then a notice says so, with a button that opens those settings. An input device with more than two channels gets a second drop-down under the source, labeled Input Channels as on the Mac, with its pairs (Channels 1-2, Channels 3-4, an odd last channel alone), kept per device; 1-2 keeps Windows' own stereo mix of the device, any other pair captures all its channels and plays those two, and a pair the device no longer has plays 1-2. The Mac's picker puts an Input Devices header above the inputs; the Slint ComboBox can't show one, so here the inputs follow the apps without a divider, until a custom drop-down in a later look pass |
| Now Playing | Same (a card under Audio Source) | The track of a media session that Windows lists (its system media controls): artwork, title and artist (the app's name when the session gives no artist), with Previous, Play or Pause, and Next, which go to that app. With All System Audio the card follows an app that plays, else the first paused one; with one app chosen, only that app's own session. An input device has no session, so the card hides, as it does while no app reports a track. The Mac reads only Spotify and Music; here any app that reports to Windows shows. The sessions are read every 2 seconds, and only while the main window is open; a receiver that shows the track shares that reading, so the sessions are listed once per 2 seconds for both. An app gets 1 second to answer for its session, which is otherwise left out until the next reading, 1 second to hand over its artwork, which is otherwise asked for again after 30 seconds while the track shows without it, and 1 second to take a button |
| Master Volume | Same | |
| Equalizer card and window | On/off, a Curve picker (presets, then saved curves; Custom when none matches), preamp, ten bands, Reset. Save, Rename and Delete for saved curves are three buttons in the bottom row, Rename and Delete disabled unless the picker shows a saved curve; the Mac has them at the end of the Curve menu, named after the curve | View > Equalizer (Ctrl+E); presets, band limits and the filters come from `audiosync-core`. It changes the captured audio once, so every speaker hears it |
| This Mac | This PC | The PC's own outputs: its speakers or line out, and headphones on its jack. USB, Bluetooth, HDMI, DisplayPort and virtual outputs are under Connected Speakers, each with its kind. A Bluetooth hands-free endpoint ("Hands-Free", the call profile) is left out on purpose: it carries mono, low-bandwidth call audio, and opening it puts the device in call mode. A headset's stereo endpoint, when it has one, is listed |
| Zones | Same: a card per zone with its toggle, a count of its speakers, the zone volume and an arrow that shows or hides its speakers; Rename Zone…, Delete Zone, and "Use (pair) Instead" for a member that plays in a stereo pair or home theater | The zone menu opens by right-click, the Menu key or the "…" button. Clicking the header turns the zone on or off, Left and Right hide and show its speakers; the arrow is left out of Tab. Rename Zone…, F2 or a double-click on the zone's name edits the name in its header, the way a speaker is renamed (see Speaker rename), with no dialog; an empty name keeps the old one. A single click on the name waits for a possible second one, then shows or hides the zone's speakers. A speaker's menu has Move to Zone (each zone, then New Zone…) and Remove from Zone. A speaker is in one zone at most and shows only in its zone's card, so This PC and Connected Speakers list the rest, and say so when every speaker of theirs is in a zone. Hidden speakers get no row and aren't counted. Saved groups became zones once, with a notice saying so |
| Connected Speakers | Same: toggle, kind icon, availability note, auto-connect mark, volume | Kind icons come from Segoe Fluent Icons; the kind is also written under the name. A USB device that Windows reports as headphones, a headset or speakers gets the headphones or speaker icon, and "USB" under its name. AirPlay speakers come from AudioSync's own sender and are listed by name with the other connected speakers. One that asks for a password reads "Asks for a code", as on the Mac: selecting it opens a prompt for the password (see AirPlay password prompt). One the sender can't play yet carries a note instead of playing: "Pairing not supported yet", "AirPlay 2 is off" (or "AirPlay 2 is off until AudioSync reopens" once File > AirPlay 2 (Experimental) is checked, since it applies from the next launch), or "Not supported" (an encryption or format it can't send); selecting it leaves it off and posts a notice saying why. With none listed it says "Looking for speakers…" for the first 20 seconds of AirPlay discovery, with no Google Cast note after it, as the tray flyout does; then the Local Network hint below takes over. While discovery is stopped it says "No connected speakers", or, in its place, the Google Cast note ("Looking for Google Cast speakers…", that none were found yet, or that the search couldn't start). Outside those 20 seconds, when the only outputs there are virtual ones it also says they're hidden and points to View > Show Virtual Devices; the Mac's note about the AirPlay engine isn't shown |
| Speaker rename | Same: Rename… in the speaker's menu, or a double-click on its name, edits the name in its row | The menu opens with a right-click, the Menu key or the "…" button. Enter applies the name, Escape keeps the old one, and a click outside every control, a click on the renamed row itself (which then leaves its delay panel alone), a rename started in another row, leaving the field for another control (Tab, or a click on a control), or the speaker going away applies it, as on the Mac; switching to another window keeps the field open. A single click on the name waits for a possible second one, then works like a click on the row. A speaker in a zone is renamed the same way in its row under In zones, by a double-click on its name or F2 on its switch. The field stops taking characters at 60; the name is trimmed and kept on one line; empty, or the device's own name, brings back the device's name. It is kept by speaker key in the settings file, shows in the row, the tray flyout, notices, the AirPlay code and password dialogs and the sync hint, and never reaches the audio side. While renamed, the name's tooltip and the toggle's accessible label give the device's own name |
| Hide Speaker | Same: last in the speaker's menu, after a separator; the reference output has none | A speaker that is on turns off first. A hidden speaker leaves the window, the tray flyout, zones (which leave it out of their count), Calibrate All and the Web Remote, isn't switched on by auto-connect or at launch, and stays hidden when it is unplugged or announced again. Its volume, delay, name and marks stay saved, and the hidden set is kept by speaker key in the settings file (`hidden_speakers`). Narrator says "name hidden" and, from Hidden Speakers, "name shown". A section whose speakers are all hidden says so and points to Hidden Speakers |
| Per-speaker delay (sync controls) | Same: −100, −10, a field, +10, +100, and the hint. In both apps a step applies at once and drops half-typed text, a typed value applies on Enter (Return on the Mac), and File > Apply Typed Delay, Ctrl+S (Command-S on the Mac) from anywhere in the window, applies every typed delay; both keep the sync controls open. A click on the speaker's delay or anywhere on its row, the open sync controls' blank space included, opens or closes its sync controls, after applying what was typed in any open one; the "No AirPlay speakers found" note under the last row is part of that row, and the note alone in an empty list is outside every row. A click outside every speaker row applies every typed delay and closes all of them. Escape drops every typed delay and closes them all, quitting drops it too, leaving the field for another control applies nothing, and text the field can't read goes back to the saved value. The field takes only digits after one leading + or −; a number out of range gets a red outline and a line with the range and what Enter applies, which its accessible description reads too, and which Narrator (VoiceOver on the Mac) says once as the number goes out of range | Local outputs only, as on the Mac. The reference (the PC's own speakers or line out: the BuiltIn output with the smallest endpoint ID) has no delay field; every other local output shows a signed offset relative to it ("+150 ms", "−250 ms"), from −2000 to +20 000 ms, that applies with or without AirPlay, and the field takes a leading + or −. The hint names the reference, or, with none, says the lowest value plays first. The earliest output taking part plays at once and the others wait for it by the difference |
| AirPlay delay | Same idea: one field in its own card under Connected Speakers, shown while the AirPlay sender runs | Both apps have it; on the Mac it is a row under the AirPlay speakers. Both show L, the sender's latency plus the stored adjustment. L follows the sender's latency, and a typed value applies on Enter or Ctrl+S (Return or Command-S on the Mac) or a click outside every speaker row, as a speaker's delay does, with the same range line (0 to 20 000 ms) (as you type, its lower bound would clamp a half-typed number), and stores the difference to the latency of that moment; Escape drops what was typed, and so does quitting, in both apps. While an AirPlay speaker is on, every local output waits L on top of its place relative to the reference |
| Device volume hint | Same: a line under the volume slider of a local speaker that's on, when its own volume in Windows is low or muted: "Windows volume is 13%" with Set to 75%, "Muted in Windows" with Unmute, or "Muted in Windows, volume 13%" with Unmute at 75%. The tray flyout's row shows a small speaker glyph instead, and its description carries the hint | Low means 25 % or less. AudioSync reads each switched-on output's volume and mute in Windows, whether or not it streams, and follows Windows' change notifications; it writes them only when you click the fix (75 % when low, never lower, and unmute when muted), and the row changes once Windows reports the change, so a device that ignores the write keeps the hint. When Windows audio doesn't answer a volume call within 5 s, the row shows no hint for now and AudioSync tries again about ten seconds later ([EC3](EDGE_CASES.md#ec3-sleep-and-wake)). The hint has no dismiss button: it goes when the volume or the mute changes, or the speaker is switched off. The glyph is in the secondary text color: Windows' own Mute glyph while muted (Segoe Fluent Icons has no speaker with a slash) and a speaker with one wave while low. AirPlay speakers and outputs that are off show nothing |
| AirPlay problem with Retry | A notice; it has Retry only when AirPlay itself stopped or couldn't start (see Notices) | A speaker that stops answering stays on while the sender keeps trying, with a notice that clears itself once it plays again or drops off the list; one that turns out to need pairing, or a password after it played or for auto-connect, is turned off with a notice saying so |
| Local Network hint | A note under Connected Speakers: "No AirPlay speakers found yet. Make sure this PC is on the same network as your speakers, and that Windows Firewall allows AudioSync on it (or set the network to Private)." | Windows has no Local Network permission, but Windows Firewall asks once, the first time AudioSync looks for AirPlay speakers, and blocks discovery when that was denied, or on a network Windows counts as Public while AudioSync is allowed on Private ones only. So the note shows once discovery has run for 20 seconds without finding a speaker, like the Mac's soft hint, under the rows when there are other connected speakers. It goes as soon as any AirPlay speaker is listed (even one AudioSync can't play yet) and comes back only after another 20 seconds without one. It never shows while discovery is stopped or couldn't start: the AirPlay notice with Retry covers that |
| Menu bar panel | Tray icon and flyout | Left click opens the flyout, double click the main window, right click a menu (Open AudioSync, Turn Streaming On/Off, Launch at Sign-In, Exit AudioSync). Escape or a click elsewhere closes the flyout. As in the Mac panel, the source drop-down and the master volume have no label above them (Narrator still reads "Audio Source" and "Master volume"), and with no speaker or zone listed the list says "Looking for speakers…" for the first 20 seconds of AirPlay discovery, as the main window does, then "No speakers found". When every speaker is hidden it says so instead, and while discovery is stopped or hasn't started it shows no text. Each zone is a row with its count, its speakers indented under it |
| Closing the window keeps the app in the menu bar | Closing it keeps the app in the tray | File > Exit (Ctrl+Q, or Exit in the tray) quits |
| Launch at Login (gear menu and app menu) | Launch at Sign-In, a check item in File (above Exit, where the Mac has it in its app menu) and in the tray menu | Windows says "sign in", so the label does too. Turning it on writes the value `AudioSync` under the current user's Run key (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, documented in Microsoft's "Run and RunOnce Registry Keys") as the quoted path of this exe plus `--background`; a command line over the documented 260 characters isn't written. Turning it off removes that value. The check mark shows what the registry says, read again every 2 seconds and when the pointer reaches the tray icon, so a change in Task Manager or Settings > Apps > Startup shows up; nothing about it is kept in the settings file. Task Manager and Settings mark a startup app turned off under `Explorer\StartupApproved\Run`, which Microsoft doesn't document: AudioSync reads it (an odd first byte means turned off, as both write it) and shows the option off, but never writes it. Turning the option on then writes the Run value, opens Settings > Apps > Startup and says in a notice to turn AudioSync on there, as the Mac does when macOS wants approval; the notice goes once Windows has it on. When Windows refuses the write or the removal, a notice says so and the check mark keeps showing what the registry has; a refused write points to the Startup folder (`shell:startup`) instead, with `--background` to add to the shortcut's Target, a refused removal to Settings > Apps > Startup, and that notice goes once Windows has the entry off there. A Run value that starts another `audiosync.exe` (the app moved, or another build wrote it) shows as on; AudioSync doesn't rewrite it by itself, since Microsoft asks programs started from the Run key not to write to it while they run, and turning the option off and on again points it at this exe. Started with `--background`, AudioSync opens in the tray without the main window, or with it when there's no tray icon. A shortcut in the Startup folder starts AudioSync without `--background` unless its Target has it, so the main window opens at sign-in; AudioSync doesn't look for such shortcuts, and the option's check mark shows only the Run value. A launch with `--background` while AudioSync already runs just exits and leaves the running copy as it is (a launch without it asks that copy to show its window, as before). The Run key and the notices are behind a small trait, so the tests use an in-memory one and never touch the registry |
| When the source app quits (AudioSync > Switch to All System Audio When the Source App Quits, a check item in the app menu) | Switch to All System Audio When the Source App Quits, a check item in File | On by default on the Mac since 0.7.1, off by default on Windows. Off: when the app being captured quits, streaming stops, as before. On: streaming goes on with All System Audio, every speaker keeps playing (AirPlay included), and a notice says the app quit and AudioSync switched. The source stays All System Audio when the app starts again; picking the app makes it the source again. Remembered across launches. It is an item of the native menu bar, so the keyboard reaches it as it does the others (Alt or F10) and it looks like them in either mode |
| Web Remote… (gear menu and Window menu) | File > Web Remote… | The window has the switch, the address and QR code, the pairing code, the paired devices and the port (1024 to 65535) |
| AirPlay 2 (Experimental) (gear menu and app menu) | File > AirPlay 2 (Experimental) | Off by default; a change applies from the next launch, as on the Mac |
| Main menu: Edit | Built into the text fields | Ctrl+C, Ctrl+V and the rest work in every field |
| Main menu: Window (Minimize, Zoom, Bring All to Front) | The title bar's own buttons | Not a Windows menu |
| Help > AudioSync Guide (the Guide window) | Help > AudioSync Guide (F1), the AudioSync Guide window | The Mac Guide's sections in its order, written for Windows: First Steps, Sync, Calibration, More Controls, AirPlay, Notices, Bluetooth, Volume, Web Remote, then Hearing Sound Twice, which only Windows has. Input devices are in First Steps with the other sources; track info on AirPlay receivers, Launch at Sign-In and View > Appearance are in More Controls, in the Mac's order; AirPlay passwords follow the Apple TV PIN in AirPlay. Notices ends with what happens when the source app quits and File > Switch to All System Audio When the Source App Quits. It says nothing about the Mac-only parts (the Home app's speaker access), and nothing about the Now Playing card. Each section is a group Narrator names by its heading. One window, 640 x 600 at first and at least 360 x 280; opening it again brings it to the front where it was, a new opening starts at the top. The arrow keys, Page Up, Page Down, Space, Home and End scroll it, Escape or Close closes it. While a double-audio notice shows, its section has Show How, which opens that notice's guide; the Guide doesn't keep its size or place across launches |
| AirPlay pairing prompt | The Pair with (speaker name) dialog, for an Apple TV that asks for a PIN | Turning such an Apple TV on (its note reads "Asks for a code", then "Pairing…") makes it show a 4-digit PIN; the dialog's title names the speaker, and the dialog takes the PIN (digits only, Enter pairs, Escape or Cancel stops the pairing and leaves the speaker off). A wrong PIN keeps the dialog open with a message, and you enter the PIN the TV shows then; any other failure closes it with a notice. A second Apple TV turned on while the dialog is open reads "Waiting to pair…" and gets the dialog once the first pairing ends. If a pairing you cancel had already finished on the TV, the pairing is kept: the speaker stays off (or turns on, if you had turned it on again meanwhile), and turning it on needs no PIN. Once paired the speaker turns on, and the pairing is kept, encrypted for the Windows user, so no PIN is asked again; Forget Pairing in the speaker's "…" menu removes it. Auto-connect never starts a pairing by itself: it posts a notice instead. An Apple TV set to ask for a code every time turns off whenever discovery lists it as asking for a code every time and it isn't playing, unless you turned it on yourself and haven't turned it off since, and while its pairing runs no session starts for it, so the PIN the TV shows stays the one to type. An Apple TV that also asks for a password asks for it during the pairing, which then starts again with it |
| AirPlay password prompt | The same Pair with (speaker name) dialog, with a password field, for a speaker protected by a password | Turning such a speaker on opens it, and so does one that asks for a password only once connected; Enter or Pair (once something is typed) sends the password and turns the speaker on, Escape or Cancel leaves it off. AudioSync keeps the password in memory until it quits, never on disk, so it asks once per launch, as the Mac does. A password the speaker turns down before it plays opens the dialog again with a warning; turned down later, or for a speaker auto-connect turned on, the speaker goes off with a notice instead. Forget Password in the speaker's "…" menu drops the password and turns the speaker off, so turning it on asks again |

## Keyboard

| Keys | Does |
|---|---|
| Tab, Shift+Tab | Move through the controls in reading order: header, notices, the left column's cards, then each speaker row |
| Space, Enter | Turn the focused speaker or zone on or off, or press the focused button |
| Arrow keys | Move the focused slider (volume in steps of 1, equalizer in 0.5 dB) |
| Menu key, Shift+F10 | Open the focused row's context menu |
| Ctrl+S | Apply every typed delay |
| Ctrl+N | New zone from the speakers that are on |
| F5 | Refresh devices |
| Ctrl+E | Equalizer |
| Ctrl+W | Close the window (AudioSync stays in the tray); in the equalizer, the AudioSync Guide, the double-audio guide, Web Remote or About, close that window; in a dialog, cancel it as Escape does |
| Ctrl+Q | Exit AudioSync, from the main window, the equalizer, either Guide, Web Remote or About |
| F1 | AudioSync Guide |
| Escape | Close the flyout, the equalizer, the AudioSync Guide, the double-audio guide, Web Remote or a dialog |

Each Mac menu shortcut has its Windows key, with Ctrl for Command: Command-S,
-W, -Q, -N and Option-Command-E become Ctrl+S, Ctrl+W, Ctrl+Q, Ctrl+N and
Ctrl+E, Command-R becomes F5 and Command-? F1, as Windows apps use them. The
menu items show them. The Mac's Hide and Hide Others have none, since Windows
doesn't hide apps; Minimize is the title bar's button (or Windows+Down), and
Command-0 brings up the main window, which on Windows is where the menu bar is.

A focused control shows a ring in the focus color while it has keyboard focus.
Escape in a dialog works wherever the focus is, on its buttons too. When an
update removes the focused control (a notice dismissed or retried, a speaker
unplugged), the main window moves the focus to the notice in its place (its
dismiss button names the notice in a few words, "Dismiss: double audio"),
the same control of a row once its list is rebuilt (a delay field keeps what
you typed), the options button of the row next to one that went, or, once
its list is empty, the options button of the other speaker list's first row,
then the first zone header (a speaker that went from a zone: that zone's header), and the
streaming switch only when no row is left, so Narrator keeps talking, Tab
goes on from there, and Space or Enter never toggles something you didn't
aim at. The tray flyout keeps
the focus inside itself the same way, so Escape still closes it.

## Narrator and contrast

Every control has an accessible role, a label and, where it has one, a value:
a speaker's toggle reads as "Desk Speaker, USB, toggle button, On" with what
selecting it does as the description, and each slider reads its speaker's
name and value. The on/off toggles (streaming, the equalizer, each speaker and
zone) are toggle buttons rather than switches, because Slint 1.18 tells
Windows that a switch can't take keyboard focus. The sections (This PC,
Zones, Connected Speakers, Notices) and each zone card are named groups, so Narrator says where
the focus is. Icons, the brand mark, section titles and values a control
already reads are decorative and hidden from Narrator. Tests in
`desktop/src/contrast.rs` fail when a control in `ui/` has no accessible
label, when one of AudioSync's own controls lacks a role, a default action
or a focus ring, when a toggle uses the switch role, when an accessible name
holds an icon glyph, or when a decorative piece would still reach Narrator.
The device-volume hint under a speaker's volume slider is one text element
that names the speaker ("Speakers: Windows volume is 13%"), and its fix is a
link-style button whose label starts with what it shows ("Set to 75%,
Speakers"); Narrator reaches both right after the slider, and the hint
appearing isn't announced. Pressed from the keyboard, the fix first hands the
focus to that slider, so it stays in the row when the hint goes. In the tray flyout the hint is part of the
speaker toggle's description.

A new notice is announced without the focus moving: each window has an
invisible polite live region (Slint 1.18's `accessible-live-region`), and
when a notice is posted the app puts "Notice: " and its short title there
("Notice: Kitchen didn't start, turned off"), in the open flyout if there
is one, otherwise in the main window, never both. It empties again after 5
seconds, or as soon as that notice is dismissed or clears itself, so
Narrator's scan mode finds nothing stray and the same words for a later
notice are announced again. Notices already there at launch aren't
announced, and neither is a notice posted again with the same title for the
same thing (an AirPlay speaker that keeps retrying is said once). Windows
only exposes a window that is showing, so with every window closed nothing
is said; for that case the tray icon's tooltip names the newest notice on a
second line, the same way, which Narrator reads when the tray icon gets the
focus (Windows+B), and drops it once no notice is left.
The tooltip is cut to the 127 characters Windows shows. AudioSync shows no
toast for a notice: an automatic event never pops up over other apps.

Text colors meet 4.5:1 on every background they're drawn on, in light and
dark, also under the hover fill of a speaker row with a hovered button in it.
The pairs are listed in `brand/tokens.json`, and a unit test in
`desktop/src/contrast.rs` computes each ratio and fails below 4.5:1; another
fails when a `.slint` file draws text in a token color that no pair covers. The focus ring reaches 3:1 on every background. Text inside the
standard controls (buttons, lists, fields) uses the Fluent style's own colors.
