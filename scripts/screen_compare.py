"""Lists every screen element that differs between two platforms' state dumps.

Each folder holds one `<STATE>-<theme>.json` per LAYAR state, written by the
Mac renderer (scripts/screenshots/render.sh --layar) or the desktop's
layar_shots test:

    {"window": [w, h], "elements": [{"key": "Kitchen", "frame": [x, y, w, h],
     "font": null | {"name": "...", "size": 13}, "fg": "#rrggbb" | null,
     "bg": "#rrggbb" | null}]}

Elements match by key (id or accessibility label); repeated keys are numbered
top to bottom, left to right. A row is printed for each element whose frame
moves more than 1 pt (a shift every element under it in its column shares is
printed once, where it starts), whose font differs, or whose colour differs by more
than one step in a channel, for each element on one side only, and for each
state on one side only, with the reason the other side's not-set-up.txt
gives for it (`ID: reason` or `ID theme: reason` per line). A field that is null on either side is not compared.

Usage: python -I scripts/screen_compare.py <mac-dir> <other-dir>
       python -I scripts/screen_compare.py --self-test
Exits 1 when it printed a row other than a `#` offset comment.
"""

import io
import json
import os
import re
import statistics
import sys

FRAME_SLACK = 1.0
# The 21 pt line of the 17 pt wordmark, the tallest one-line element. Up to
# this a width and height come from the platform's font or, for an icon
# button, from the Mac's glyph against the other platform's hit area.
ONE_LINE = 22
# One step of 8-bit colour sampled from anti-aliased pixels: below this the
# two renderers' rounding differs, not the colour.
COLOUR_STEP = 8
# Caption buttons Windows draws in the window's own top row, at its right
# edge; the Mac's sit in a title bar outside the content the renderer photographs.
WINDOW_CHROME = {"Minimize", "Maximize", "Close"}
CAPTION_HEIGHT = 40
# Three 46 px Windows 11 caption buttons.
CAPTION_WIDTH = 3 * 46
# The other platform's label templates that name its own system where the
# Mac's name macOS, and its Exit for the Mac's Quit.
PLATFORM_LABELS = [
    (re.compile(r"^(.+): (?:Windows|PipeWire) volume is (.+)$"), r"\1: macOS volume is \2"),
    (re.compile(r"^(?:Windows|PipeWire) volume of (.+)$"), r"macOS volume of \1"),
    (re.compile(r"^Exit AudioSync$"), "Quit AudioSync"),
]

# The header's status line, keyed by its role: its text differs by platform
# (the Mac adds the AirPlay delay), so the text is compared apart.
STATUS_LINE = re.compile(r"^Streaming to \d+ speakers?")


def load(folder):
    states = {}
    for name in sorted(os.listdir(folder)):
        if name.endswith(".json"):
            with open(os.path.join(folder, name), encoding="utf-8") as file:
                states[name[: -len(".json")]] = json.load(file)
    return states


def not_set_up(folder):
    """Reasons from the folder's not-set-up.txt, by `ID` or `ID theme`."""
    reasons = {}
    path = os.path.join(folder, "not-set-up.txt")
    if os.path.exists(path):
        with open(path, encoding="utf-8") as file:
            for line in file:
                named, _, reason = line.strip().partition(": ")
                if reason:
                    reasons[named] = reason
    return reasons


def missing(state, present, absent):
    base, _, theme = state.rpartition("-")
    reasons = not_set_up(absent)
    reason = reasons.get(f"{base} {theme}") or reasons.get(base)
    return f"not set up in {absent}: {reason}" if reason else f"only in {present}"


def as_mac(state):
    """The other platform's elements under the Mac's keys, without its caption buttons."""
    width = state["window"][0]
    out = []
    for element in state["elements"]:
        x, y, w, h = element["frame"]
        if element["key"] in WINDOW_CHROME and y + h <= CAPTION_HEIGHT and x >= width - CAPTION_WIDTH - 1:
            continue
        key = element["key"]
        for template, mac in PLATFORM_LABELS:
            key = template.sub(mac, key)
        out.append(dict(element, key=key))
    return out


def keyed(elements):
    elements = [
        dict(e, key="Status line", text=e["key"]) if STATUS_LINE.match(e["key"]) else e for e in elements
    ]
    ordered = sorted(elements, key=lambda e: (e["frame"][1], e["frame"][0]))
    seen = {}
    out = {}
    for element in ordered:
        count = seen.get(element["key"], 0) + 1
        seen[element["key"]] = count
        out[element["key"] if count == 1 else f'{element["key"]}#{count}'] = element
    return out


def colour(text):
    return tuple(int(text[i : i + 2], 16) for i in (1, 3, 5))


def moved(a, b, shifts):
    """Whether b sits elsewhere than a: across by its leading edge or centre,
    down by its centre less the shift of the nearest element above it in its
    column (`shifts`, filled in top to bottom), and in size above one line only."""
    (ax, ay, aw, ah), (bx, by, bw, bh) = a["frame"], b["frame"]
    down = by + bh / 2 - ay - ah / 2
    column = [(y, shift) for (x, y, w, _), shift in shifts if y < ay and x < ax + aw and ax < x + w]
    nearest = max((y for y, _ in column), default=None)
    above = [shift for y, shift in column if y == nearest]
    shifts.append((a["frame"], down))
    if min(abs(bx - ax), abs(bx + bw / 2 - ax - aw / 2)) > FRAME_SLACK:
        return True
    if min(abs(down - shift) for shift in [0, *above]) > FRAME_SLACK:
        return True
    return min(ah, bh) > ONE_LINE and (abs(bw - aw) > FRAME_SLACK or abs(bh - ah) > FRAME_SLACK)


def changes(a, b, frame_moved):
    found = []
    if a.get("text", "") != b.get("text", ""):
        found.append(f'text {a["text"]} vs {b["text"]}')
    if frame_moved:
        found.append(f'frame {fmt(a["frame"])} vs {fmt(b["frame"])}')
    fa, fb = a.get("font"), b.get("font")
    if fa and fb and (fa["name"] != fb["name"] or fa["size"] != fb["size"]):
        found.append(f'font {fa["name"]} {fa["size"]} vs {fb["name"]} {fb["size"]}')
    for field in ("fg", "bg"):
        ca, cb = a.get(field), b.get(field)
        if ca and cb and max(abs(x - y) for x, y in zip(colour(ca), colour(cb))) > COLOUR_STEP:
            found.append(f"{field} {ca} vs {cb}")
    return found


def fmt(frame):
    return "[" + ",".join(f"{v:g}" for v in frame) + "]"


def compare(left_dir, right_dir):
    left, right = load(left_dir), load(right_dir)
    rows = []
    for state in sorted(set(left) | set(right)):
        if state not in right:
            rows.append(f"{state}\t-\t{missing(state, left_dir, right_dir)}")
            continue
        if state not in left:
            rows.append(f"{state}\t-\t{missing(state, right_dir, left_dir)}")
            continue
        a, b = keyed(left[state]["elements"]), keyed(as_mac(right[state]))
        # A renderer that puts the whole content lower (a taller title bar) shifts
        # every element; the median shift of the paired elements is taken out first
        # and printed as a `#` comment row, which readers of the rows skip.
        common = set(a) & set(b)
        offset = statistics.median(b[k]["frame"][1] - a[k]["frame"][1] for k in common) if common else 0
        if offset:
            rows.append(f"# {state}\toffset y {offset:+g}")
        aligned = {k: dict(e, frame=[e["frame"][0], e["frame"][1] - offset, *e["frame"][2:]]) for k, e in b.items()}
        shifts = []
        frames = {
            key: moved(a[key], aligned[key], shifts)
            for key in sorted(set(a) & set(b), key=lambda k: (a[k]["frame"][1], a[k]["frame"][0]))
        }
        for key in sorted(set(a) | set(b)):
            if key not in b:
                rows.append(f"{state}\t{key}\tonly in {left_dir}")
            elif key not in a:
                rows.append(f"{state}\t{key}\tonly in {right_dir}")
            else:
                found = changes(a[key], b[key], frames[key])
                if found:
                    rows.append(f"{state}\t{key}\t" + "; ".join(found))
    return rows


def self_test():
    here = os.path.dirname(os.path.abspath(__file__))
    fixtures = os.path.join(here, "..", "Tests", "fixtures", "screen_compare")
    mac, other = os.path.join(fixtures, "mac"), os.path.join(fixtures, "other")
    got = [row.replace(mac, "mac").replace(other, "other") for row in compare(mac, other)]
    want = [
        "MAIN-EMPTY-terang\t-\tonly in mac",
        "MAIN-IDLE-terang\tKitchen\tframe [20,100,200,22] vs [20,102,200,22]",
        "MAIN-IDLE-terang\tKitchen#2\tfg #1d1d1f vs #3a3a3c",
        "MAIN-IDLE-terang\tSpeakers\tonly in mac",
        "MAIN-IDLE-terang\tStatus line\ttext Streaming to 2 speakers · AirPlay delay 1.8 s vs Streaming to 2 speakers",
        "MAIN-IDLE-terang\tStreaming to\tfont SF Pro 13 vs Segoe UI 13",
        "MAIN-IDLE-terang\tWindows Laptop\tonly in other",
        "MAIN-IDLE-terang\t−170\tonly in mac",
        "MAIN-ROWS-terang\tAlpha\tframe [450,100,32,32] vs [450,106,32,32]",
        "MAIN-ROWS-terang\tDen\tframe [256,160,32,32] vs [256,156,32,32]",
        "MAIN-ROWS-terang\tGamma\tframe [450,220,32,32] vs [450,226,32,32]",
        "MAIN-ROWS-terang\tNotice\tframe [20,300,400,60] vs [20,300,400,80]",
        "# MAIN-SHIFT-terang\toffset y +4",
        "TRAY-IDLE-gelap\t-\tnot set up in mac: the panel is not drawn here",
    ]
    if got != want:
        print("FAIL screen_compare self-test")
        for line in got:
            print("  got  " + line)
        for line in want:
            print("  want " + line)
        return 1
    if compare(mac, mac):
        print("FAIL screen_compare: a folder differs from itself")
        return 1
    # A Windows console's stdout is cp1252, which has no U+2212 minus sign.
    console = io.TextIOWrapper(io.BytesIO(), encoding="cp1252")
    shown, sys.stdout = sys.stdout, console
    try:
        main([mac, other])
        console.flush()
        failed = "−170".encode() not in console.buffer.getvalue()
    except UnicodeEncodeError as error:
        failed = error
    finally:
        sys.stdout = shown
    if failed:
        print(f"FAIL screen_compare: rows are not written as UTF-8 on a cp1252 console ({failed})")
        return 1
    print("ok screen_compare self-test")
    return 0


def main(argv):
    if argv == ["--self-test"]:
        return self_test()
    if len(argv) != 2:
        print(__doc__.strip().split("\n\n")[-1], file=sys.stderr)
        return 2
    rows = compare(argv[0], argv[1])
    # `python -I` ignores PYTHONIOENCODING, and keys carry U+2212.
    sys.stdout.flush()
    sys.stdout.buffer.write("".join(row + "\n" for row in rows).encode("utf-8"))
    sys.stdout.flush()
    print(f"{len(rows)} differences", file=sys.stderr)
    return 1 if any(not row.startswith("#") for row in rows) else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
