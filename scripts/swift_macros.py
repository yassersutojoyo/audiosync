#!/usr/bin/env python3
"""Fails when a tracked Swift file uses a SwiftUI macro.

The Command Line Tools toolchain ships without SwiftUI's macro plugins, so
@State, @Entry, @Previewable and #Preview stop the Mac build there, while the
macOS CI runners build with Xcode and pass. Hold view state in an
ObservableObject with @StateObject instead. Line comments and string
literals are not checked.

Usage:
    scripts/swift_macros.py              check every tracked Swift file
    scripts/swift_macros.py FILE...      check the given files
    scripts/swift_macros.py --self-test  check the scanner
"""

import argparse
import os
import re
import subprocess
import sys
import tempfile

_MACRO = re.compile(r"(?<![\w.])(@(?:SwiftUI\.)?(?:State|Entry|Previewable)|#(?:SwiftUI\.)?Preview)\b")
_STRING = re.compile(r'"(?:\\.|[^"\\])*"')


def hits_in(text):
    hits = []
    for number, line in enumerate(text.splitlines(), 1):
        code = _STRING.sub('""', line).split("//", 1)[0]
        for match in _MACRO.finditer(code):
            hits.append((number, match.group(1)))
    return hits


def tracked_files(root):
    out = subprocess.run(["git", "-C", root, "ls-files", "-z", "--", "*.swift"],
                         check=True, capture_output=True).stdout
    return [p for p in out.decode("utf-8").split("\0") if p]


def check(root, paths):
    found = []
    for path in paths:
        full = os.path.join(root, path)
        if not path.endswith(".swift") or not os.path.isfile(full):
            continue
        with open(full, encoding="utf-8") as f:
            for line, name in hits_in(f.read()):
                found.append((path, line, name))
    return found


def self_test():
    failures = []
    samples = {
        "    @State private var drawn: Float = 0\n": [(1, "@State")],
        "    @StateObject private var drawn = MeterDrawn()\n": [],
        "    /// An object rather than @State: the Command Line Tools\n": [],
        "    let x = 1 // not @State here\n": [],
        "#Preview { MainWindow() }\n": [(1, "#Preview")],
        "    @Entry var tint = Color.teal\n": [(1, "@Entry")],
        "    @Previewable @State var on = false\n": [(1, "@Previewable"), (1, "@State")],
        "    @Environment(\\.accessibilityReduceMotion) var reduce\n": [],
        "    @FocusState private var focused: Bool\n": [],
        "    let s = \"x@State\"\n": [],
        "    let u = \"http://x\"; @State var y = 0\n": [(1, "@State")],
        "    @SwiftUI.State private var on = false\n": [(1, "@SwiftUI.State")],
        "": [],
    }
    for text, want in samples.items():
        got = hits_in(text)
        if got != want:
            failures.append(f"scan of {text!r}: expected {want}, got {got}")
    files = {
        "Sources/A.swift": b"struct A: View {\n    @State var on = false\n}\n",
        "Sources/B.swift": b"// @State in a comment\n",
        "notes.txt": b"@State\n",
    }
    with tempfile.TemporaryDirectory() as root:
        for path, data in files.items():
            os.makedirs(os.path.join(root, os.path.dirname(path)), exist_ok=True)
            with open(os.path.join(root, path), "wb") as f:
                f.write(data)
        got = check(root, sorted(files) + ["Sources/Gone.swift"])
    if got != [("Sources/A.swift", 2, "@State")]:
        failures.append(f"check of sample files: got {got}")
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    print(f"self-test: {len(samples) + 1} cases, {len(failures)} failure(s)")
    return 1 if failures else 0


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("files", nargs="*")
    args = parser.parse_args(argv)
    if args.self_test:
        return self_test()
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    if args.files:
        paths = [os.path.relpath(os.path.abspath(p), root).replace(os.sep, "/") for p in args.files]
    else:
        paths = tracked_files(root)
    found = check(root, paths)
    for path, line, name in found:
        print(f"{path}:{line}: {name} needs SwiftUI's macro plugins, which the Command Line Tools lack")
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
