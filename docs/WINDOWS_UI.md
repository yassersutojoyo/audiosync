# The Windows app's screens

The Windows app (`windows/`) has the same screens as the Mac app, in the same
order, with Windows manners: a menu bar with Ctrl shortcuts, a notification-area
(tray) icon with a flyout where the Mac has its menu bar panel, the system's
window controls, and light or dark following Windows. The wording follows the
Mac app's `AppText`; all of it lives in `windows/ui/strings.slint`.

Colors, the wordmark typeface, spacing and corner radii come from the shared
design tokens (`brand/tokens.json`, see `brand/README.md`).

The windows read their data through the `Backend` trait in
`windows/src/ui_model.rs`. On Windows that is the adapter over the WASAPI audio
backend (`windows/src/adapter.rs`), which captures and plays for real. On other
hosts, where the UI is worked on, a mock with example speakers stands in:
nothing plays and no audio device is opened there.

Settings live in memory for now: the speakers you turned on, their delays and
volumes, groups, auto-connect marks, the source and the equalizer start over
at every launch.

## Mac main window to Windows

| Mac main window | Windows | Notes |
|---|---|---|
| Header: mark, wordmark, status line | Same | The mark is grey while streaming is off, as on the Mac |
| Streaming switch | Switch in the header; Speakers > Turn Streaming On/Off; the tray menu | |
| Options (gear) menu: New Group, Refresh Devices | Speakers menu: New Group from Active Speakers… (Ctrl+N), Refresh Devices (F5) | Windows keeps commands in the menu bar |
| Options menu: Appearance | Not offered | The app follows the Windows light or dark mode |
| Options menu: Show Virtual Devices | View > Show Virtual Devices | Off by default, as on the Mac, but the Mac remembers the choice and Windows turns it off again at every launch (nothing is saved yet): a virtual output (a loopback driver, a voice changer) is listed only while this is on or while the output is turned on |
| Options menu: Launch at Login, Guide, Check for Updates | Later | They need saved settings, the Windows startup-apps setting, a Windows guide and an update channel |
| Notices | Same, each with a dismiss button, plus Dismiss All | The tray flyout shows the newest one and "n more notices…". Windows posts them for streaming turned on with no speaker, an app that quit or isn't running, capture that failed, a speaker that couldn't start, a speaker that was disconnected (that one clears itself once the speaker is back; a speaker whose stream only restarts, after a format change, gets none), and a source you may hear twice because it also plays straight from an output. That last one names the outputs you can mute apart from the ones AudioSync plays to as well, clears itself once the cause is gone, and once dismissed stays away for the same source and outputs ([how it's detected](WINDOWS_AUDIO_ROUTING.md#detection)) |
| Audio Source | Same (a drop-down list) | All System Audio (everything but AudioSync), then each app that plays audio, by name. The list is read again every few seconds and with Refresh Devices; the chosen app stays chosen while it's paused or after it quits, and streaming finds it whenever it runs (restarted too), even before it plays again |
| Now Playing | Later | Needs the Windows media session API |
| Master Volume | Same | |
| Equalizer card and window | Same: on/off, presets (Custom when none matches), preamp, ten bands, Reset | View > Equalizer (Ctrl+E); presets, band limits and the filters come from `audiosync-core`. It changes the captured audio once, so every speaker hears it |
| This Mac | This PC | The PC's own outputs: its speakers or line out, and headphones on its jack. USB, Bluetooth, HDMI, DisplayPort and virtual outputs are under Connected Speakers, each with its kind. A Bluetooth hands-free endpoint ("Hands-Free", the call profile) is left out on purpose: it carries mono, low-bandwidth call audio, and opening it puts the device in call mode. A headset's stereo endpoint, when it has one, is listed |
| Groups | Same: toggle, members, Rename Group… and Delete Group | In a context menu: right-click, the Menu key, or the "…" button |
| Connected Speakers | Same: toggle, kind icon, availability note, auto-connect mark, volume | Kind icons come from Segoe Fluent Icons. With none listed it says "No connected speakers"; the Mac's note about the AirPlay engine isn't shown, since Windows has no AirPlay yet |
| Per-speaker delay (sync controls) | Same: −100, −10, a field that applies as you type, +10, +100, and the hint | Local outputs only, as on the Mac |
| AirPlay problem with Retry | Later | Comes with the Windows AirPlay engine; until then nothing in the Windows app mentions AirPlay |
| Local Network hint | Not needed | Windows has no Local Network permission (the firewall may ask once the AirPlay engine runs) |
| Menu bar panel | Tray icon and flyout | Left click opens the flyout, double click the main window, right click a menu (Open AudioSync, Turn Streaming On/Off, Exit AudioSync). Escape or a click elsewhere closes the flyout |
| Closing the window keeps the app in the menu bar | Closing it keeps the app in the tray | File > Exit (or Exit in the tray) quits |
| Main menu: Edit | Built into the text fields | Ctrl+C, Ctrl+V and the rest work in every field |
| Main menu: Window (Minimize, Zoom, Bring All to Front) | The title bar's own buttons | Not a Windows menu |
| AirPlay pairing prompt | Later | With the AirPlay engine |

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
| Escape | Close the flyout, the equalizer or a dialog |

A focused control shows a ring in the focus color while it has keyboard focus.

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
or a focus ring, when a toggle uses the switch role, or when a decorative
piece would still reach Narrator.

Text colors meet 4.5:1 on every background they're drawn on, in light and
dark (the lowest is 5.07:1). The pairs are listed in `brand/tokens.json`, and
a unit test in `windows/src/contrast.rs` computes each ratio and fails below
4.5:1; another fails when a `.slint` file draws text in a token color that no
pair covers. The focus ring reaches 3:1 on every background. Text inside the
standard controls (buttons, lists, fields) uses the Fluent style's own colors.
