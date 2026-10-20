#!/usr/bin/env python3
"""Fails when public documentation contains an em dash (U+2014).

Scope is the tracked text people outside the project read: Markdown at the
repository root, windows/README.md, docs/*.md and everything under site/.
docs/*_SPEC.md is left out because the specs quote test output verbatim.
App strings, code and comments are out of scope.

Usage:
    scripts/text_check.py              check every tracked file in scope
    scripts/text_check.py FILE...      check the given files (scope still applies)
    scripts/text_check.py --self-test  check the scope rules and the scanner
"""

import argparse
import os
import re
import subprocess
import sys

EM_DASH = "—"
_SPEC = re.compile(r"docs/[^/]+_SPEC\.md")


def in_scope(path):
    if path.startswith("site/"):
        return True
    if path == "windows/README.md":
        return True
    if "/" not in path:
        return path.endswith(".md")
    if path.startswith("docs/") and path.count("/") == 1 and path.endswith(".md"):
        return not _SPEC.fullmatch(path)
    return False


def hits_in(text):
    return [n for n, line in enumerate(text.splitlines(), 1) if EM_DASH in line]


def tracked_files(root):
    out = subprocess.run(
        ["git", "-C", root, "ls-files", "-z"], capture_output=True, check=True
    ).stdout
    return [p for p in out.decode("utf-8").split("\0") if p]


def check(root, paths):
    found = []
    for path in paths:
        if not in_scope(path):
            continue
        full = os.path.join(root, path)
        if not os.path.isfile(full):
            continue
        with open(full, "rb") as f:
            data = f.read()
        try:
            text = data.decode("utf-8")
        except UnicodeDecodeError:
            # Images and fonts under site/ can't hold readable text.
            continue
        found += [(path, n) for n in hits_in(text)]
    return found


def self_test():
    failures = []
    scope = {
        "README.md": True,
        "CHANGELOG.md": True,
        "windows/README.md": True,
        "docs/DEVELOPMENT.md": True,
        "docs/ENGINE_SPEC.md": False,
        "docs/AIRPLAY_SPEC.md": False,
        "docs/sub/NOTES.md": False,
        "site/index.html": True,
        "site/icons/manifest.json": True,
        "core/README.md": False,
        "Sources/AudioSync/Localization.swift": False,
        "notes.txt": False,
    }
    for path, want in scope.items():
        if in_scope(path) != want:
            failures.append(f"scope of {path}: expected {want}")
    samples = {
        "## [0.4.1] " + EM_DASH + " 2026-10-17\n": [1],
        "first line\nsecond " + EM_DASH + " here\nthird\n": [2],
        "en dash – and hyphen - are fine\n": [],
        "": [],
    }
    for text, want in samples.items():
        got = hits_in(text)
        if got != want:
            failures.append(f"scan of {text!r}: expected {want}, got {got}")
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    print(f"self-test: {len(scope) + len(samples)} cases, {len(failures)} failure(s)")
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
        paths = [os.path.relpath(os.path.abspath(p), root).replace("\\", "/") for p in args.files]
    else:
        paths = tracked_files(root)
    found = check(root, paths)
    for path, line in found:
        print(f"{path}:{line}: em dash; use a comma, colon, parentheses or two sentences")
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
