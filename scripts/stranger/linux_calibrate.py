#!/usr/bin/env python3
"""Drives calibration through the digital loop linux_calibrate.sh builds, and
checks the record it leaves.

    linux_calibrate.py <work> <pid> <debug log> <port>
    linux_calibrate.py --check <work>

The drive switches the speakers on through the web remote, then over AT-SPI
runs one speaker's calibration (done, cancelled, failed) and Calibrate All
(Before you start, Include Anyway, cancelled, applied, then once more to see
the speakers in time), photographing each state as CAL-*.png. The check
compares every applied number with the measured one and with the saved
settings, and writes calibrate-record.txt. Steps the session can't reach go
to not-set-up.txt.
"""

import importlib.util
import json
import os
import re
import subprocess
import sys
import time

missing = []


def note(line):
    print(line, flush=True)


def skip(state, why):
    note(f"not reached: {state} ({why})")
    missing.append(f"{state}: {why}")


def remote_module(here):
    spec = importlib.util.spec_from_file_location("remote", os.path.join(here, "remote.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def drive(work, pid, log_path, port):
    import gi

    gi.require_version("Atspi", "2.0")
    from gi.repository import Atspi, GLib

    here = os.path.dirname(os.path.abspath(__file__))
    remote_lib = remote_module(here)
    remote = remote_lib.Remote(port)

    def log_text():
        try:
            with open(log_path, encoding="utf-8", errors="replace") as log:
                return log.read()
        except (FileNotFoundError, TypeError):
            return ""

    def wait_log(pattern, after, seconds):
        deadline = time.time() + seconds
        while time.time() < deadline:
            found = re.search(pattern, log_text()[after:])
            if found:
                return found.group(0)
            time.sleep(1)
        return None

    def paired():
        code = remote_lib.pairing_code(log_path)
        if not code:
            return None
        try:
            status, text = remote.call("POST", "/api/pair", {"code": code})
        except OSError:
            return None
        return status == 200 and text

    remote_lib.wait("a pairing", 60, paired)

    def state():
        status, text = remote.call("GET", "/api/state")
        return json.loads(text) if status == 200 else {}

    wanted = ["Speakers", "Desk Speaker", "Far Speaker", "Headset", "Stranger Receiver"]

    def all_listed():
        names = {s["name"]: s for s in state().get("speakers", []) if s.get("available")}
        return names if all(n in names for n in wanted) else None

    try:
        listed = remote_lib.wait("every loop speaker listed", 60, all_listed)
    except SystemExit as error:
        note(str(error))
        listed = {s["name"]: s for s in state().get("speakers", [])}
    for name in wanted:
        if name in listed:
            path = "/api/speakers/" + remote_lib.urllib.parse.quote(listed[name]["id"], safe="")
            status, text = remote.call("POST", path, {"on": True})
            note(f"remote: switch on {name}: {status}")
        else:
            skip("speaker on", f"{name} not listed")
    time.sleep(4)
    for item in state().get("speakers", []):
        note(f"remote: speaker {item['name']!r} kind={item['kind']} on={item['on']}")

    def application():
        for _ in range(30):
            desktop = Atspi.get_desktop(0)
            for i in range(desktop.get_child_count()):
                app = desktop.get_child_at_index(i)
                try:
                    if app and (app.get_process_id() == pid or "audiosync" in (app.get_name() or "").lower()):
                        return app
                except GLib.Error:
                    continue
            time.sleep(1)
        return None

    def walk(node, depth=0):
        yield node
        if depth < 40:
            for i in range(node.get_child_count()):
                child = node.get_child_at_index(i)
                if child:
                    yield from walk(child, depth + 1)

    def shown(app):
        out = []
        for i in range(app.get_child_count()):
            window = app.get_child_at_index(i)
            if window and window.get_state_set().contains(Atspi.StateType.SHOWING):
                out.append(window)
        return out

    def named(app, name, within=None):
        roots = [within] if within is not None else shown(app)
        return [n for r in roots for n in walk(r) if n.get_name() == name]

    def actionable(nodes):
        for node in nodes:
            actions = node.get_action_iface()
            if actions and actions.get_n_actions() > 0:
                return node
        return None

    def press(app, name, state_name, within=None, seconds=15):
        deadline = time.time() + seconds
        while time.time() < deadline:
            node = actionable(named(app, name, within))
            if node is not None:
                node.get_action_iface().do_action(0)
                time.sleep(1.5)
                return True
            time.sleep(0.5)
        skip(state_name, f"no element named '{name}' with an action")
        return False

    # The main window's row is a panel of the same name; the sheet holds Before you start or Results.
    def sheet(app):
        for node in named(app, "Calibrate All Speakers"):
            if node.get_role() in (Atspi.Role.PANEL, Atspi.Role.GROUPING) and any(
                n.get_name() in ("Before you start", "Results", "Cancel") for n in walk(node)
            ):
                return node
        return None

    def shot(state_name):
        path = os.path.join(work, f"{state_name}.png")
        subprocess.run(["import", "-window", "root", path], check=True)
        note(f"shot: {os.path.basename(path)}")

    def tree(app, file_name):
        with open(os.path.join(work, file_name), "w", encoding="utf-8") as out:
            for window in shown(app):
                for node in walk(window):
                    name = node.get_name()
                    if name:
                        out.write(f"{node.get_role_name()} '{name}'\n")

    def mark():
        return len(log_text())

    app = application()
    if app is None or not shown(app):
        skip("CAL-READY", "no window on the accessibility bus")
        shot("CAL-SCREEN")
        return
    shot("CAL-READY")
    tree(app, "atspi-ready.txt")

    # One speaker: done, cancelled, failed.
    if press(app, "Show controls for Desk Speaker delay", "CAL-ONE-READY"):
        shot("CAL-ONE-READY")
        at = mark()
        if press(app, "Calibrate with Microphone, Desk Speaker", "CAL-ONE-RUNNING", seconds=5):
            shot("CAL-ONE-RUNNING")
            line = wait_log(r"calibration of \S*desk\S*[^\n]*", at, 90)
            note(f"one speaker, desk: {line}")
            time.sleep(2)
            shot("CAL-ONE-DONE")
            tree(app, "atspi-one-done.txt")
        at = mark()
        if press(app, "Calibrate with Microphone, Desk Speaker", "CAL-ONE-CANCELLED", seconds=5):
            time.sleep(1)
            press(app, "Cancel calibrating Desk Speaker", "CAL-ONE-CANCELLED", seconds=5)
            time.sleep(2)
            shot("CAL-ONE-CANCELLED")
            tree(app, "atspi-one-cancelled.txt")
            line = wait_log(r"calibration[^\n]*(?:cancel|Cancel)[^\n]*", at, 5)
            note(f"one speaker, cancel: {line}")
    if press(app, "Show controls for Far Speaker delay", "CAL-ONE-FAILED"):
        at = mark()
        if press(app, "Calibrate with Microphone, Far Speaker", "CAL-ONE-FAILED", seconds=5):
            line = wait_log(r"calibration of \S*far\S*[^\n]*", at, 90)
            note(f"one speaker, far: {line}")
            time.sleep(2)
            shot("CAL-ONE-FAILED")
            tree(app, "atspi-one-failed.txt")

    def open_sheet(state_name):
        for _ in range(2):
            if not press(app, "Calibrate All Speakers", state_name):
                return None
            for _ in range(20):
                found = sheet(app)
                if found is not None:
                    return found
                time.sleep(0.5)
        skip(state_name, "no Calibrate All Speakers sheet")
        return None

    def start(found, state_name):
        starts = [n for n in walk(found) if n.get_name() == "Calibrate All Speakers" and n is not found]
        node = actionable(starts)
        if node is None:
            skip(state_name, "no start button in the sheet")
            return False
        node.get_action_iface().do_action(0)
        time.sleep(3)
        return True

    # Calibrate All: Before you start with the reference low and its Set to 75%, Include Anyway, then cancelled.
    at = mark()
    subprocess.run(["pactl", "set-sink-volume", "stranger-speakers", "20%"], check=False)
    low = wait_log(r"stranger-speakers. device volume: fine → low[^\n]*", at, 15)
    note(f"reference at 20%: {low}")
    found = open_sheet("CAL-ALL-BEFORE")
    if found is not None:
        shot("CAL-ALL-BEFORE")
        tree(app, "atspi-all-before.txt")
        at = mark()
        if press(app, "Set to 75%, Speakers", "CAL-ALL-VOLUME-FIXED", within=found, seconds=5):
            fine = wait_log(r"stranger-speakers. device volume: low → fine[^\n]*", at, 15)
            note(f"set to 75%: {fine}")
            volume = subprocess.run(["pactl", "get-sink-volume", "stranger-speakers"], capture_output=True, text=True)
            note(f"pactl after Set to 75%: {volume.stdout.strip()}")
            shot("CAL-ALL-VOLUME-FIXED")
            tree(app, "atspi-all-volume-fixed.txt")
        # Back to full scale so the loop's levels match the runs before this step.
        subprocess.run(["pactl", "set-sink-volume", "stranger-speakers", "100%"], check=False)
        if press(app, "Include Anyway, Headset", "CAL-ALL-INCLUDED", within=found):
            shot("CAL-ALL-INCLUDED")
        at = mark()
        if start(found, "CAL-ALL-CANCELLED"):
            time.sleep(3)
            shot("CAL-ALL-RUNNING")
            press(app, "Cancel", "CAL-ALL-CANCELLED", within=found, seconds=5)
            line = wait_log(r"Calibrate All (?:stopped|ended)[^\n]*", at, 20)
            note(f"all, cancel: {line}")
            time.sleep(2)
            shot("CAL-ALL-CANCELLED")
            tree(app, "atspi-all-cancelled.txt")
        press(app, "Close the calibration results", "CAL-ALL-CANCELLED", within=found, seconds=3)

    # Calibrate All to the end, Apply, then Done.
    found = open_sheet("CAL-ALL-RESULTS")
    if found is not None:
        press(app, "Include Anyway, Headset", "CAL-ALL-RESULTS", within=found)
        at = mark()
        if start(found, "CAL-ALL-RESULTS"):
            line = wait_log(r"Calibrate All ended[^\n]*", at, 240)
            note(f"all: {line}")
            time.sleep(2)
            shot("CAL-ALL-RESULTS")
            tree(app, "atspi-all-results.txt")
            at = mark()
            if press(app, "Measure Again, Desk Speaker", "CAL-ALL-MEASURED-AGAIN", within=found, seconds=5):
                line = wait_log(r"Measure Again of \S+ started[\s\S]*?Calibrate All ended[^\n]*", at, 120)
                note(f"all, measure again: {line and line[-200:]}")
                time.sleep(2)
                shot("CAL-ALL-MEASURED-AGAIN")
                tree(app, "atspi-all-measured-again.txt")
            at = mark()
            if press(app, "Apply", "CAL-ALL-APPLIED", within=found, seconds=5):
                line = wait_log(r"Calibrate All (?:applied|: Apply refused)[^\n]*", at, 10)
                note(f"all, apply: {line}")
                shot("CAL-ALL-APPLIED")
                tree(app, "atspi-all-applied.txt")
            press(app, "Done with the calibration results", "CAL-ALL-APPLIED", within=found, seconds=3)

    # Once more after Apply: in time means each speaker's new delay is its current one.
    time.sleep(3)
    found = open_sheet("CAL-ALL-AGAIN")
    if found is not None:
        press(app, "Include Anyway, Headset", "CAL-ALL-AGAIN", within=found)
        at = mark()
        if start(found, "CAL-ALL-AGAIN"):
            line = wait_log(r"Calibrate All ended[^\n]*", at, 240)
            note(f"all, again: {line}")
            time.sleep(2)
            shot("CAL-ALL-AGAIN")
            tree(app, "atspi-all-again.txt")
        press(app, "Close the calibration results", "CAL-ALL-AGAIN", within=found, seconds=3)
    tree(app, "atspi-end.txt")


def check(work):
    """The record: what the loop measured and what was applied and saved."""
    try:
        with open(os.path.join(work, "audiosync-debug.log"), encoding="utf-8", errors="replace") as log:
            text = log.read()
    except FileNotFoundError:
        print("FAIL: no debug log")
        return 1
    try:
        with open(os.path.join(work, "settings-after.json"), encoding="utf-8") as saved:
            settings = json.load(saved)
    except (FileNotFoundError, json.JSONDecodeError):
        settings = {}
    outputs = settings.get("outputs", {})
    lines = ["Calibration through a digital loop (null sinks into a virtual microphone in PipeWire, the fake",
             "AirPlay receiver played into it): it proves the flow and the numbers, not a room's acoustics.", ""]
    failed = []

    def section(title, pattern):
        found = re.findall(pattern, text)
        lines.append(f"{title}:")
        lines.extend(f"  {line}" for line in found or ["(none)"])
        return found

    section("microphone refused (Linux has no prompt; PipeWire refuses with EPERM)", r"[^\n]*(?:Denied|Missing)[^\n]*")
    single = section("one speaker", r"calibration (?:of|refused)[^\n]*")
    section("Calibrate All plan", r"Calibrate All (?:plan|started)[^\n]*")
    section("Calibrate All end", r"Calibrate All (?:ended|stopped)[^\n]*")
    applied = section("Calibrate All applied", r"Calibrate All (?:applied|: Apply refused)[^\n]*")

    lines.append("applied against measured and saved:")
    ended = re.findall(r"Calibrate All ended[^\n]*", text)
    for line in applied:
        for target, new in re.findall(r'target: (Speaker\("[^"]+"\)|AirPlay), old_ms: [^,]+, new_ms: (-?[\d.]+)', line):
            new_ms = float(new)
            measured = any(re.search(rf"{re.escape(new)}\b", e) for e in ended)
            saved = None
            name = re.match(r'Speaker\("([^"]+)"\)', target)
            if name:
                saved = outputs.get(name.group(1).removeprefix("local:"), {}).get("delay_ms")
            else:
                saved = settings.get("airplay_adjustment_ms")
            lines.append(f"  {target}: applied {new_ms} ms, in the measured sheet: {measured}, saved: {saved}")
            if not measured:
                failed.append(f"{target}: the applied {new_ms} ms is not in the measured sheet")
            if name and (saved is None or abs(float(saved) - new_ms) > 0.5):
                failed.append(f"{target}: saved {saved}, applied {new_ms}")
    for line in single:
        found = re.search(r"calibration of (\S+): .* offset \S+ -> (-?[\d.]+) ms", line)
        if found:
            lines.append(f"  one speaker {found.group(1)}: measured offset {found.group(2)} ms")
    # In time: the run after Apply finds every clearly measured speaker within 5 ms of its delay.
    if ended:
        lines.append("the run after Apply:")
        for key, difference in re.findall(r'id: "([^"]+)"[^}]*?difference_ms: (-?[\d.]+)', ended[-1]):
            lines.append(f"  {key}: still {float(difference):.1f} ms off")
            if abs(float(difference)) > 5.0:
                failed.append(f"{key} is {float(difference):.1f} ms off after Apply")
    fixed = section("Set to 75% on the low reference", r"[^\n]*stranger-speakers\) device volume(?: fix)?: [^\n]*(?:75%|→)[^\n]*")
    if not any("low → fine (75%" in line for line in fixed):
        failed.append("Set to 75% did not bring the reference from low to 75%")
    section("Measure Again", r"Measure Again of [^\n]*")
    if not re.search(r"Measure Again of local:stranger-desk started(?:(?!Calibrate All started)[\s\S])*?Calibrate All ended", text):
        failed.append("Measure Again of the desk speaker did not start and run to its end")
    lines.append("")
    if not applied:
        failed.append("Calibrate All was never applied")
    if len(ended) < 2:
        failed.append("Calibrate All did not run to its end twice")
    if re.search(r"panic", text, re.IGNORECASE):
        failed.append("the debug log has a panic")
    lines.extend(f"FAIL: {f}" for f in failed)
    lines.append("PASS" if not failed else "")
    with open(os.path.join(work, "calibrate-record.txt"), "w", encoding="utf-8") as out:
        out.write("\n".join(lines) + "\n")
    print("\n".join(lines))
    return 1 if failed else 0


def main(argv):
    if argv and argv[0] == "--check":
        return check(argv[1])
    work, pid, log_path, port = argv[0], int(argv[1]), argv[2], int(argv[3])
    try:
        drive(work, pid, log_path, port)
    finally:
        with open(os.path.join(work, "not-set-up.txt"), "a", encoding="utf-8") as out:
            out.writelines(line + "\n" for line in missing)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
