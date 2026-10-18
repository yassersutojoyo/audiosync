# The Windows app's screens

The Windows app (`windows/`) has the same screens as the Mac app, in the same
order, with Windows manners: a menu bar with Ctrl shortcuts, a notification-area
(tray) icon with a flyout where the Mac has its menu bar panel, the system's
window controls, and light or dark following Windows. The wording follows the
Mac app's `AppText`; all of it lives in `windows/ui/strings.slint`.

Colors, the wordmark typeface, spacing and corner radii come from the shared
design tokens (`brand/tokens.json`, see `brand/README.md`).

The windows read their data through the `Backend` trait in
`windows/src/ui_model.rs`. Until the WASAPI backend is connected, a mock with
example speakers stands in: nothing plays and no audio device is opened.

## Mac main window to Windows

| Mac main window | Windows | Notes |
|---|---|---|
| Header: mark, wordmark, status line | Same | The mark is grey while streaming is off, as on the Mac |
| Streaming switch | Switch in the header; Speakers > Turn Streaming On/Off; the tray menu | |
| Options (gear) menu: New Group, Refresh Devices | Speakers menu: New Group from Active Speakers… (Ctrl+N), Refresh Devices (F5) | Windows keeps commands in the menu bar |
| Options menu: Appearance | Not offered | The app follows the Windows light or dark mode |
| Options menu: Show Virtual Devices, Launch at Login, Guide, Check for Updates | Later | They need the real backend, the Windows startup-apps setting, a Windows guide and an update channel |
| Notices | Same, each with a dismiss button, plus Dismiss All | The tray flyout shows the newest one and "n more notices…" |
| Audio Source | Same (a drop-down list) | |
| Now Playing | Later | Needs the Windows media session API |
| Master Volume | Same | |
| Equalizer card and window | Same: on/off, presets (Custom when none matches), preamp, ten bands, Reset | View > Equalizer (Ctrl+E); presets and band limits come from `audiosync-core` |
| This Mac | This PC | The PC's own outputs (speakers, headphones) |
| Groups | Same: toggle, members, Rename Group… and Delete Group | In a context menu: right-click, the Menu key, or the "…" button |
| Connected Speakers | Same: toggle, kind icon, availability note, auto-connect mark, volume | Kind icons come from Segoe Fluent Icons |
| Per-speaker delay (sync controls) | Same: −100, −10, a field that applies as you type, +10, +100, and the hint | Local outputs only, as on the Mac |
| AirPlay problem with Retry | Later | Comes with the Windows AirPlay engine; until then the empty note shows |
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
