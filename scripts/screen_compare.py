"""Lists every screen element that differs between two platforms' state dumps.

Each folder holds one `<STATE>-<theme>.json` per LAYAR state, written by the
Mac renderer (scripts/screenshots/render.sh --layar) or the desktop's
layar_shots test:

    {"window": [w, h], "elements": [{"key": "Kitchen", "frame": [x, y, w, h],
     "font": null | {"name": "...", "size": 13}, "fg": "#rrggbb" | null,
     "bg": "#rrggbb" | null}]}

Elements match by key (id or accessibility label); repeated keys are numbered
top to bottom, left to right. A row is printed for each element whose frame
moves more than 1 pt, whose font differs, or whose colour differs by more
than one step in a channel, for each element on one side only, and for each
state on one side only. A field that is null on either side is not compared.

Usage: python -I scripts/screen_compare.py <mac-dir> <other-dir>
       python -I scripts/screen_compare.py --self-test
Exits 1 when it printed a row.
"""

import json
import os
import sys

FRAME_SLACK = 1.0
# One step of 8-bit colour sampled from anti-aliased pixels: below this the
# two renderers' rounding differs, not the colour.
COLOUR_STEP = 8


def load(folder):
    states = {}
    for name in sorted(os.listdir(folder)):
        if name.endswith(".json"):
            with open(os.path.join(folder, name), encoding="utf-8") as file:
                states[name[: -len(".json")]] = json.load(file)
    return states


def keyed(elements):
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


def changes(a, b):
    found = []
    if any(abs(x - y) > FRAME_SLACK for x, y in zip(a["frame"], b["frame"])):
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
            rows.append(f"{state}\t-\tonly in {left_dir}")
            continue
        if state not in left:
            rows.append(f"{state}\t-\tonly in {right_dir}")
            continue
        a, b = keyed(left[state]["elements"]), keyed(right[state]["elements"])
        for key in sorted(set(a) | set(b)):
            if key not in b:
                rows.append(f"{state}\t{key}\tonly in {left_dir}")
            elif key not in a:
                rows.append(f"{state}\t{key}\tonly in {right_dir}")
            else:
                found = changes(a[key], b[key])
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
        "MAIN-IDLE-terang\tStreaming to\tfont SF Pro 13 vs Segoe UI 13",
        "TRAY-IDLE-gelap\t-\tonly in other",
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
    print("ok screen_compare self-test")
    return 0


def main(argv):
    if argv == ["--self-test"]:
        return self_test()
    if len(argv) != 2:
        print(__doc__.strip().split("\n\n")[-1], file=sys.stderr)
        return 2
    rows = compare(argv[0], argv[1])
    for row in rows:
        print(row)
    print(f"{len(rows)} differences", file=sys.stderr)
    return 1 if rows else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
