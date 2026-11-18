#!/usr/bin/env python3
"""Drives the installed app through one screen group over AT-SPI and
photographs each state, for scripts/stranger/linux_shots.sh.

Each state becomes <STATE>-<theme>.png in the work folder, named as the
Mac's LAYAR photos where a state matches; one the session can't reach gets
a line in not-set-up.txt as screen_compare.py reads it, and
atspi-<theme>.txt holds the tree as AT-SPI saw it. The nowplaying group
exits 1 when the Now Playing card lacks Shuffle or Repeat drawn on, or its
cover reads as one colour.

    linux_shots.py <work> <group> <theme> <pid>
"""

import os
import subprocess
import sys
import time

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi, GLib  # noqa: E402

WORK, GROUP, THEME, PID = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
missing = []
failed = []


def skip(state, why):
    print(f"not reached: {state} {THEME} ({why})")
    missing.append(f"{state} {THEME}: {why}")


def application(seconds=30):
    deadline = time.time() + seconds
    while time.time() < deadline:
        desktop = Atspi.get_desktop(0)
        for i in range(desktop.get_child_count()):
            app = desktop.get_child_at_index(i)
            try:
                if app and (app.get_process_id() == PID or "audiosync" in (app.get_name() or "").lower()):
                    return app
            except GLib.Error:
                continue
        time.sleep(1)
    return None


def windows(app):
    shown = []
    for i in range(app.get_child_count()):
        window = app.get_child_at_index(i)
        if window and window.get_state_set().contains(Atspi.StateType.SHOWING):
            shown.append(window)
    return shown


def walk(node, depth=0):
    yield node, depth
    if depth < 40:
        for i in range(node.get_child_count()):
            child = node.get_child_at_index(i)
            if child:
                yield from walk(child, depth + 1)


def find(app, name, seconds=10):
    """The first element named exactly `name`, one with an action first."""
    deadline = time.time() + seconds
    while True:
        shown = windows(app)
        for window in shown:
            if window.get_name() == name:
                return window
        named = [n for w in shown for n, _ in walk(w) if n.get_name() == name]
        for node in named:
            actions = node.get_action_iface()
            if actions and actions.get_n_actions() > 0:
                return node
        if named:
            return named[0]
        if time.time() > deadline:
            return None
        time.sleep(0.5)


def use(app, name, state):
    node = find(app, name)
    if node is None:
        skip(state, f"no element named '{name}' (see atspi-{THEME}.txt)")
        return False
    actions = node.get_action_iface()
    if not actions or actions.get_n_actions() == 0:
        skip(state, f"'{name}' offers no action")
        return False
    actions.do_action(0)
    time.sleep(2)
    return True


def extents(node):
    """Elements without the Component interface (labels, some groups) have no box."""
    try:
        return node.get_extents(Atspi.CoordType.SCREEN)
    except GLib.GError:
        return None


def shot(node, state):
    path = os.path.join(WORK, f"{state}-{THEME}.png")
    crop = []
    if node is not None:
        box = extents(node)
        if box is not None and box.width > 0 and box.height > 0:
            crop = ["-crop", f"{box.width}x{box.height}+{max(box.x, 0)}+{max(box.y, 0)}", "+repage"]
    subprocess.run(["import", "-window", "root", *crop, path], check=True)
    print(f"shot: {os.path.basename(path)}")


def pixels(box, *measure):
    """What ImageMagick's `measure` reads from the screen's `box` (w, h, x, y)."""
    width, height, x, y = box
    grab = os.path.join(os.environ.get("TMPDIR", "/tmp"), f"measure-{THEME}.png")
    subprocess.run(["import", "-window", "root", "-crop", f"{width}x{height}+{x}+{y}", "+repage", grab],
                   check=True)
    return subprocess.run(["convert", grab, *measure], check=True, capture_output=True, text=True).stdout


def save_tree(app):
    with open(os.path.join(WORK, f"atspi-{THEME}.txt"), "w", encoding="utf-8") as out:
        for node, depth in walk(app):
            box = extents(node)
            where = f"{box.x},{box.y} {box.width}x{box.height}" if box is not None else "no-box"
            out.write(f"{'  ' * depth}{node.get_role_name()} '{node.get_name()}' {where}\n")


def open_tray():
    """A click on the icon where the host draws it, else Activate as a host sends it."""
    where = os.environ.get("TRAY_ICON_AT")
    if where:
        x, y = where.split()
        subprocess.run(["xdotool", "mousemove", x, y, "click", "1"], check=True)
        return f"click at {x},{y}"
    names = subprocess.run(
        ["dbus-send", "--session", "--print-reply", "--dest=org.freedesktop.DBus", "/org/freedesktop/DBus",
         "org.freedesktop.DBus.ListNames"], capture_output=True, text=True, check=True).stdout
    item = next((w.strip('"') for w in names.split() if "StatusNotifierItem" in w), None)
    if item is None:
        return None
    subprocess.run(["dbus-send", "--session", "--type=method_call", f"--dest={item}", "/StatusNotifierItem",
                    "org.kde.StatusNotifierItem.Activate", "int32:1200", "int32:12"], check=True)
    return f"Activate on {item}"


def run(app, main):
    if GROUP == "main":
        shot(main, "MAIN-IDLE")
        if use(app, "Presets", "MAIN-PRESETS-EMPTY"):
            shot(main, "MAIN-PRESETS-EMPTY")
            use(app, "Presets", "MAIN-PRESETS-EMPTY")
        if use(app, "Options", "MAIN-SETTINGS") and use(app, "Settings…", "MAIN-SETTINGS"):
            settings = find(app, "Settings")
            if settings is not None and settings.get_role() in (Atspi.Role.FRAME, Atspi.Role.WINDOW):
                shot(settings, "MAIN-SETTINGS")
            else:
                skip("MAIN-SETTINGS", "no Settings window")
    elif GROUP in ("delay", "calibrate"):
        # The two null sinks linux_shots.sh made, turned on as the Mac's Desk Speaker and Headset are.
        on = use(app, "Desk Speaker", "MAIN-DELAY-ROWS") and use(app, "Headset", "MAIN-DELAY-ROWS")
        if GROUP == "delay" and on:
            time.sleep(2)
            shot(main, "MAIN-DELAY-ROWS")
        if GROUP == "calibrate" and use(app, "Calibrate All Speakers", "CAL-SHEET"):
            shot(main, "CAL-SHEET")
            time.sleep(15)
            shot(main, "CAL-SHEET-LATER")
    elif GROUP == "tray":
        how = open_tray()
        if how is None:
            skip("TRAY-IDLE", "no StatusNotifier item on the session bus")
            return
        print(f"tray: {how}")
        time.sleep(2)
        flyout = next((w for w in windows(app) if w.get_name() != main.get_name()), None)
        shot(None, "TRAY-SCREEN")
        if flyout is None:
            skip("TRAY-IDLE", f"no panel after the {how}")
        else:
            shot(flyout, "TRAY-IDLE")
    elif GROUP == "notices":
        # Streaming on with no speaker on: the No Speaker notice.
        if use(app, "Streaming", "NOTICE-START"):
            time.sleep(2)
            shot(main, "NOTICE-START")
    elif GROUP == "nowplaying":
        # mpris_player.py plays a patterned cover with Shuffle on and LoopStatus Playlist.
        buttons = {name: find(app, name, seconds=20) for name in ("Shuffle", "Repeat", "Previous Track")}
        card = find(app, "Now Playing")
        for name, node in buttons.items():
            if node is None:
                print(f"FAIL: no {name} on the Now Playing card ({THEME})")
                failed.append(name)
        if card is None:
            print(f"FAIL: no Now Playing card ({THEME})")
            failed.append("card")
            return
        time.sleep(2)
        shot(card, "NOW-PLAYING")
        box = extents(card)
        # The cover is the card's top square; 10 px in clears its rounded corners.
        side = box.width - 20
        colours = int(pixels((side, side, box.x + 10, box.y + 10), "-format", "%k", "info:"))
        print(f"cover: {colours} colours ({THEME})")
        if colours < 16:
            print(f"FAIL: the cover reads as one colour ({THEME})")
            failed.append("cover")
        # An active button's glyph is teal; an idle one is grey, as Previous Track is.
        for name, node in buttons.items():
            if node is None:
                continue
            b = extents(node)
            tint = float(pixels((b.width, b.height, b.x, b.y), "-colorspace", "HSL", "-channel", "G",
                                "-separate", "-format", "%[fx:maxima]", "info:"))
            print(f"{name}: strongest saturation {tint:.2f} ({THEME})")
            if (tint >= 0.3) != (name != "Previous Track"):
                print(f"FAIL: {name} is not drawn {'on' if name != 'Previous Track' else 'idle'} ({THEME})")
                failed.append(name)


def main():
    app = application()
    shown = windows(app) if app else []
    if not shown:
        skip("MAIN-IDLE", "no window on the accessibility bus within 30 s")
        shot(None, "SCREEN")
    else:
        time.sleep(3)
        run(app, shown[0])
        save_tree(app)
    with open(os.path.join(WORK, "not-set-up.txt"), "a", encoding="utf-8") as out:
        out.writelines(line + "\n" for line in missing)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
