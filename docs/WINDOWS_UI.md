# The Windows app's screens

The Windows app (`windows/`) has the same screens as the Mac app, in the same
order, with Windows manners: a menu bar with Ctrl shortcuts, a notification-area
(tray) icon with a flyout where the Mac has its menu bar panel, the system's
window controls, and light or dark following Windows. The wording follows the
Mac app's `UIStrings`; all of it lives in `windows/ui/strings.slint`.

Colors, the wordmark typeface, font sizes and the window margins come from the
shared design tokens (`brand/tokens.json`, see `brand/README.md`). The rest
follows Windows 11 and lives only in the Windows app (the `Fluent` values in
`windows/ui/widgets.slint`): text in Segoe UI Variable, 8 px corners on cards,
notices and the flyout and 4 px on buttons and rows, 1 px card edges, hover and
pressed fills, and short state changes (83 ms). Every titled window (main,
equalizer, both guides, About, the group dialog and the Apple TV code dialog) asks Windows for Mica in its
title bar (Windows 11 22H2 and later), on content in the token background,
which is close to Mica's own tint; the tray flyout gets the system's rounded frame, border and shadow.
Where Windows doesn't offer one, the window keeps the plain frame (and the
flyout its own drawn edge). The dialogs (About, the group dialog and the
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
right click offers Undo, Redo, Cut, Copy, Paste and Select all.

The About dialog (Help > About AudioSync) shows the brand mark and the
wordmark on top, then the version, the audio core's version, the copyright and
the license, and the Slint badge small at the bottom.

The main window has a header (mark, wordmark, status and the Streaming switch),
then a left column of cards (Audio Source, Master Volume, Equalizer) and the
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
same note under the name. A group row waits too, with the same note, when
every present member waits (it shows the first present member's note).

The windows read their data through the `Backend` trait in
`windows/src/ui_model.rs`. On Windows that is the adapter over the WASAPI audio
backend (`windows/src/adapter.rs`), which captures and plays for real. On other
hosts, where the UI is worked on, a mock with example speakers stands in:
nothing plays and no audio device is opened there.

Settings are kept across launches in `%LOCALAPPDATA%\AudioSync\settings.json`
(`windows/src/settings.rs`): the source, each speaker's switch, delay and
volume, the master volume, auto-connect marks, groups, the equalizer, View >
Show Virtual Devices and the main window's size and place (fitted to the
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
| Options (gear) menu: New Group, Refresh Devices | Speakers menu: New Group from Active Speakers… (Ctrl+N), Refresh Devices (F5) | Windows keeps commands in the menu bar |
| Options menu: Appearance | Not offered | The app follows the Windows light or dark mode |
| Options menu: Show Virtual Devices | View > Show Virtual Devices | Off by default and remembered, as on the Mac: a virtual output (a loopback driver, a voice changer) is listed only while this is on or while the output is turned on. Once listed it stays listed for the session (until the app restarts or this setting changes), so turning it off doesn't pull its row away |
| Notices | Same, each with a dismiss button, plus Dismiss All | The tray flyout shows the newest one and "n more notices…". Windows posts them for streaming turned on with no speaker, an app that quit or isn't running, an input device that isn't connected, was disconnected or is blocked by the microphone privacy settings, capture that failed (when Windows audio restarts mid-stream, AudioSync first tries to capture again by itself for a few seconds, and the notice comes only if that doesn't work or the capture is lost again right away), a speaker that couldn't start, a speaker that was disconnected (that one clears itself once the speaker is back, also when a USB speaker comes back in another USB port, where it keeps its settings; a speaker whose stream only restarts, after a format change or a wake, gets none), a speaker another app holds for itself in exclusive mode (it stays on and AudioSync tries it again every few seconds; the notice says to close that app or stop apps from taking exclusive control of the speaker in Sound settings, and clears itself once the speaker plays, streaming stops or the speaker is unplugged), AirPlay that stopped after an internal error or couldn't start, or couldn't start looking for speakers, at launch too (its Retry starts it again), an AirPlay speaker that's on while Windows has no working sound output, so apps have nowhere to play (it clears itself once an output is back or no AirPlay speaker is on), settings or Apple TV pairings that couldn't be saved, so changes made now won't be kept after AudioSync quits (told once while saving keeps failing, cleared once a save works again), settings or Apple TV pairings that couldn't be read at launch, so AudioSync started without them, once the old file has been kept aside (told once, with the copy's name), and a source you may hear twice because it also plays straight from an output. That last one names the outputs you can mute apart from the ones AudioSync plays to as well, clears itself once the cause is gone, and once dismissed stays away for the same source and outputs ([how it's detected](WINDOWS_AUDIO_ROUTING.md#detection)). Its Show How button opens a short guide that fits the case: for one app, send it to an output you don't listen to in the Volume mixer; for the whole system, make such an output the Windows default (and move any app that plays straight to another output). Muting is offered only when every output named is one AudioSync doesn't play to, and the guide warns that muting the app itself silences AudioSync too. Its buttons open the Volume mixer or Sound settings; AudioSync never changes those settings itself |
| Audio Source | Same (a drop-down list) | All System Audio (everything but AudioSync), then each app that plays audio, by name, then each connected input device (a microphone, line in), by name. A Bluetooth headset's hands-free microphone isn't offered, since opening it switches the headset to call quality. The list is read again every few seconds and with Refresh Devices; the chosen app stays chosen while it's paused or after it quits, and streaming finds it whenever it runs (restarted too), even before it plays again. A chosen input device stays chosen while it's unplugged: streaming stops with a notice when it's pulled out, and starts again only once it's back. Windows' microphone privacy settings can block desktop apps; then a notice says so, with a button that opens those settings. The Mac's picker puts an Input Devices header above the inputs; the Slint ComboBox can't show one, so here the inputs follow the apps without a divider, until a custom drop-down in a later look pass |
| Now Playing | Same (a card under Audio Source) | The track of a media session that Windows lists (its system media controls): artwork, title and artist (the app's name when the session gives no artist), with Previous, Play or Pause, and Next, which go to that app. With All System Audio the card follows an app that plays, else the first paused one; with one app chosen, only that app's own session. An input device has no session, so the card hides, as it does while no session counts. The Mac reads only Spotify and Music; here any app that reports to Windows shows. The sessions are read every 2 seconds, and only while the main window is open |
| Master Volume | Same | |
| Equalizer card and window | Same: on/off, presets (Custom when none matches), preamp, ten bands, Reset | View > Equalizer (Ctrl+E); presets, band limits and the filters come from `audiosync-core`. It changes the captured audio once, so every speaker hears it |
| This Mac | This PC | The PC's own outputs: its speakers or line out, and headphones on its jack. USB, Bluetooth, HDMI, DisplayPort and virtual outputs are under Connected Speakers, each with its kind. A Bluetooth hands-free endpoint ("Hands-Free", the call profile) is left out on purpose: it carries mono, low-bandwidth call audio, and opening it puts the device in call mode. A headset's stereo endpoint, when it has one, is listed |
| Groups | Same: toggle, members, Rename Group… and Delete Group | In a context menu: right-click, the Menu key, or the "…" button |
| Connected Speakers | Same: toggle, kind icon, availability note, auto-connect mark, volume | Kind icons come from Segoe Fluent Icons; the kind is also written under the name. A USB device that Windows reports as headphones, a headset or speakers gets the headphones or speaker icon, and "USB" under its name. AirPlay speakers come from AudioSync's own sender and are listed by name with the other connected speakers. One that asks for a password reads "Asks for a code", as on the Mac: selecting it opens a prompt for the password (see AirPlay password prompt). One the sender can't play yet carries a note instead of playing: "Pairing not supported yet", "AirPlay 2 not supported yet", or "Not supported" (an encryption or format it can't send); selecting it leaves it off and posts a notice saying why. With none listed it says "No connected speakers", and when the only outputs there are virtual ones it also says they're hidden and points to View > Show Virtual Devices; the Mac's note about the AirPlay engine isn't shown |
| Per-speaker delay (sync controls) | Same: −100, −10, a field, +10, +100, and the hint. In both apps a step applies at once and drops half-typed text, a typed value applies on Enter (Return on the Mac), and File > Apply Typed Delay, Ctrl+S (Command-S on the Mac) from anywhere in the window, applies every typed delay; both keep the sync controls open. A click on the speaker's delay or anywhere on its row opens or closes its sync controls, after applying what was typed in any open one; the "No AirPlay speakers found" note under the last row is part of that row, and the note alone in an empty list is outside every row. A click outside every speaker row applies every typed delay and closes all of them. Escape drops every typed delay and closes them all, quitting drops it too, leaving the field for another control applies nothing, and text the field can't read goes back to the saved value. The field takes only digits after one leading + or −; a number out of range gets a red outline and a line with the range and what Enter applies, which its accessible description reads too, and which Narrator (VoiceOver on the Mac) says once as the number goes out of range | Local outputs only, as on the Mac. The reference (the PC's own speakers or line out: the BuiltIn output with the smallest endpoint ID) has no delay field; every other local output shows a signed offset relative to it ("+150 ms", "−250 ms"), from −2000 to +20 000 ms, that applies with or without AirPlay, and the field takes a leading + or −. The hint names the reference, or, with none, says the lowest value plays first. The earliest output taking part plays at once and the others wait for it by the difference |
| AirPlay delay | Same idea: one field in its own card under Connected Speakers, shown while the AirPlay sender runs | Both apps have it; on the Mac it is a row under the AirPlay speakers. Both show L, the sender's latency plus the stored adjustment. L follows the sender's latency, and a typed value applies on Enter or Ctrl+S (Return or Command-S on the Mac) or a click outside every speaker row, as a speaker's delay does, with the same range line (0 to 20 000 ms) (as you type, its lower bound would clamp a half-typed number), and stores the difference to the latency of that moment; Escape drops what was typed, and so does quitting, in both apps. While an AirPlay speaker is on, every local output waits L on top of its place relative to the reference |
| Device volume hint | Same: a line under the volume slider of a local speaker that's on, when its own volume in Windows is low or muted: "Windows volume is 13%" with Set to 75%, "Muted in Windows" with Unmute, or "Muted in Windows, volume 13%" with Unmute at 75%. The tray flyout's row shows a small speaker glyph instead, and its description carries the hint | Low means 25 % or less. AudioSync reads each switched-on output's volume and mute in Windows, whether or not it streams, and follows Windows' change notifications; it writes them only when you click the fix (75 % when low, never lower, and unmute when muted), and the row changes once Windows reports the change, so a device that ignores the write keeps the hint. When Windows audio doesn't answer a volume call within 5 s, the row shows no hint for now and AudioSync tries again about ten seconds later ([EC3](EDGE_CASES.md#ec3-sleep-and-wake)). The hint has no dismiss button: it goes when the volume or the mute changes, or the speaker is switched off. The glyph is in the secondary text color: Windows' own Mute glyph while muted (Segoe Fluent Icons has no speaker with a slash) and a speaker with one wave while low. AirPlay speakers and outputs that are off show nothing |
| AirPlay problem with Retry | A notice; it has Retry only when AirPlay itself stopped or couldn't start (see Notices) | A speaker that stops answering stays on while the sender keeps trying, with a notice that clears itself once it plays again; one that turns out to need pairing, or a password after it played or for auto-connect, is turned off with a notice saying so |
| Local Network hint | A note under Connected Speakers: "No AirPlay speakers found yet. Make sure this PC is on the same network as your speakers, and that Windows Firewall allows AudioSync on it (or set the network to Private)." | Windows has no Local Network permission, but Windows Firewall asks once, the first time AudioSync looks for AirPlay speakers, and blocks discovery when that was denied, or on a network Windows counts as Public while AudioSync is allowed on Private ones only. So the note shows once discovery has run for 20 seconds without finding a speaker, like the Mac's soft hint, under the rows when there are other connected speakers. It goes as soon as any AirPlay speaker is listed (even one AudioSync can't play yet) and comes back only after another 20 seconds without one. It never shows while discovery is stopped or couldn't start: the AirPlay notice with Retry covers that |
| Menu bar panel | Tray icon and flyout | Left click opens the flyout, double click the main window, right click a menu (Open AudioSync, Turn Streaming On/Off, Launch at Sign-In, Exit AudioSync). Escape or a click elsewhere closes the flyout. As in the Mac panel, the source drop-down and the master volume have no label above them (Narrator still reads "Audio Source" and "Master volume"), and with no speaker or group the list says "No speakers found" |
| Closing the window keeps the app in the menu bar | Closing it keeps the app in the tray | File > Exit (or Exit in the tray) quits |
| Launch at Login (gear menu and app menu) | Launch at Sign-In, a check item in File (above Exit, where the Mac has it in its app menu) and in the tray menu | Windows says "sign in", so the label does too. Turning it on writes the value `AudioSync` under the current user's Run key (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, documented in Microsoft's "Run and RunOnce Registry Keys") as the quoted path of this exe plus `--background`; a command line over the documented 260 characters isn't written. Turning it off removes that value. The check mark shows what the registry says, read again every 2 seconds and when the pointer reaches the tray icon, so a change in Task Manager or Settings > Apps > Startup shows up; nothing about it is kept in the settings file. Task Manager and Settings mark a startup app turned off under `Explorer\StartupApproved\Run`, which Microsoft doesn't document: AudioSync reads it (an odd first byte means turned off, as both write it) and shows the option off, but never writes it. Turning the option on then writes the Run value, opens Settings > Apps > Startup and says in a notice to turn AudioSync on there, as the Mac does when macOS wants approval; the notice goes once Windows has it on. When Windows refuses the write or the removal, a notice says so and the check mark keeps showing what the registry has; a refused write points to the Startup folder (`shell:startup`) instead, with `--background` to add to the shortcut's Target, a refused removal to Settings > Apps > Startup, and that notice goes once Windows has the entry off there. A Run value that starts another `audiosync.exe` (the app moved, or another build wrote it) shows as on; AudioSync doesn't rewrite it by itself, since Microsoft asks programs started from the Run key not to write to it while they run, and turning the option off and on again points it at this exe. Started with `--background`, AudioSync opens in the tray without the main window, or with it when there's no tray icon. A shortcut in the Startup folder starts AudioSync without `--background` unless its Target has it, so the main window opens at sign-in; AudioSync doesn't look for such shortcuts, and the option's check mark shows only the Run value. A launch with `--background` while AudioSync already runs just exits and leaves the running copy as it is (a launch without it asks that copy to show its window, as before). The Run key and the notices are behind a small trait, so the tests use an in-memory one and never touch the registry |
| Main menu: Edit | Built into the text fields | Ctrl+C, Ctrl+V and the rest work in every field |
| Main menu: Window (Minimize, Zoom, Bring All to Front) | The title bar's own buttons | Not a Windows menu |
| Help > AudioSync Guide (the Guide window) | Help > AudioSync Guide (F1), the AudioSync Guide window | The Mac Guide's sections in its order, written for Windows: First Steps, Sync, More Controls, AirPlay, Notices, Bluetooth, Volume, then Hearing Sound Twice, which only Windows has. It says nothing about the Mac-only parts (Appearance, the Home app), and not yet about input devices, Now Playing, Launch at Sign-In or AirPlay passwords, which came to Windows later. Each section is a group Narrator names by its heading. One window, 640 x 600 at first and at least 360 x 280; opening it again brings it to the front where it was, a new opening starts at the top. The arrow keys, Page Up, Page Down, Space, Home and End scroll it, Escape or Close closes it. While a double-audio notice shows, its section has Show How, which opens that notice's guide; the Guide doesn't keep its size or place across launches |
| AirPlay pairing prompt | The Pair with (speaker name) dialog, for an Apple TV that asks for a PIN | Turning such an Apple TV on (its note reads "Asks for a code", then "Pairing…") makes it show a 4-digit PIN; the dialog's title names the speaker, and the dialog takes the PIN (digits only, Enter pairs, Escape or Cancel stops the pairing and leaves the speaker off). A wrong PIN keeps the dialog open with a message, and the TV shows a new PIN; any other failure closes it with a notice. A second Apple TV turned on while the dialog is open reads "Waiting to pair…" and gets the dialog once the first pairing ends. If a pairing you cancel had already finished on the TV, the pairing is kept: the speaker stays off (or turns on, if you had turned it on again meanwhile), and turning it on needs no PIN. Once paired the speaker turns on, and the pairing is kept, encrypted for the Windows user, so no PIN is asked again; Forget Pairing in the speaker's "…" menu removes it. Auto-connect never starts a pairing by itself: it posts a notice instead. An Apple TV that also asks for a password asks for it during the pairing, which then starts again with it. AirPlay 2 comes with a later stage of the sender |
| AirPlay password prompt | The same Pair with (speaker name) dialog, with a password field, for a speaker protected by a password | Turning such a speaker on opens it, and so does one that asks for a password only once connected; Enter or Pair (once something is typed) sends the password and turns the speaker on, Escape or Cancel leaves it off. AudioSync keeps the password in memory until it quits, never on disk, so it asks once per launch, as the Mac does. A password the speaker turns down before it plays opens the dialog again with a warning; turned down later, or for a speaker auto-connect turned on, the speaker goes off with a notice instead |

## Keyboard

| Keys | Does |
|---|---|
| Tab, Shift+Tab | Move through the controls in reading order: header, notices, source, volume, equalizer, then each speaker row |
| Space, Enter | Turn the focused speaker or group on or off, or press the focused button |
| Arrow keys | Move the focused slider (volume in steps of 1, equalizer in 0.5 dB) |
| Menu key, Shift+F10 | Open the focused row's context menu |
| Ctrl+N | New group from the speakers that are on |
| F5 | Refresh devices |
| Ctrl+E | Equalizer |
| Ctrl+W | Close the window (AudioSync stays in the tray) |
| F1 | AudioSync Guide |
| Escape | Close the flyout, the equalizer, the AudioSync Guide, the double-audio guide or a dialog |

A focused control shows a ring in the focus color while it has keyboard focus.
Escape in a dialog works wherever the focus is, on its buttons too. When an
update removes the focused control (a notice dismissed or retried, a speaker
unplugged), the main window moves the focus to the notice in its place (its
dismiss button names the notice in a few words, "Dismiss: double audio"),
the same control of a row once its list is rebuilt (a delay field keeps what
you typed), the options button of the row next to one that went, or, once
its list is empty, the options button of the other speaker list's first row,
then of the first group (a group that went: the first speaker's), and the
streaming switch only when no row is left, so Narrator keeps talking, Tab
goes on from there, and Space or Enter never toggles something you didn't
aim at. The tray flyout keeps
the focus inside itself the same way, so Escape still closes it.

## Narrator and contrast

Every control has an accessible role, a label and, where it has one, a value:
a speaker's toggle reads as "Desk Speaker, USB, toggle button, On" with what
selecting it does as the description, and each slider reads its speaker's
name and value. The on/off toggles (streaming, the equalizer, each speaker and
group) are toggle buttons rather than switches, because Slint 1.18 tells
Windows that a switch can't take keyboard focus. The sections (This PC,
Groups, Connected Speakers, Notices) are named groups, so Narrator says where
the focus is. Icons, the brand mark, section titles and values a control
already reads are decorative and hidden from Narrator. Tests in
`windows/src/contrast.rs` fail when a control in `ui/` has no accessible
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
dark (the lowest is 5.07:1). The pairs are listed in `brand/tokens.json`, and
a unit test in `windows/src/contrast.rs` computes each ratio and fails below
4.5:1; another fails when a `.slint` file draws text in a token color that no
pair covers. The focus ring reaches 3:1 on every background. Text inside the
standard controls (buttons, lists, fields) uses the Fluent style's own colors.
