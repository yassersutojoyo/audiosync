#!/usr/bin/env python3
"""Fails when public documentation contains an em dash (U+2014).

HTML entities for it (&mdash;, &#8212;, &#x2014;) count too, and a text file
in scope that isn't valid UTF-8 fails rather than being skipped unread.

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
import tempfile

EM_DASH = "—"
# Browsers render &#151; and &#x97; as an em dash too (windows-1252 remapping).
_ENTITY = re.compile(
    r"&(mdash;|#0*(8212|151)(?![0-9])|#x0*(2014|97)(?![0-9a-f]))", re.IGNORECASE
)
_BINARY = (".png", ".ico", ".jpg", ".jpeg", ".gif", ".webp", ".icns", ".pdf",
           ".woff", ".woff2", ".ttf", ".otf")
_HINT = "em dash; use a comma, colon, parentheses or two sentences"
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
    return [
        n
        for n, line in enumerate(text.splitlines(), 1)
        if EM_DASH in line or _ENTITY.search(line)
    ]


def tracked_files(root):
    out = subprocess.run(
        ["git", "-C", root, "ls-files", "-z"], capture_output=True, check=True
    ).stdout
    return [p for p in out.decode("utf-8").split("\0") if p]


def check(root, paths):
    found = []
    for path in paths:
        if not in_scope(path) or path.lower().endswith(_BINARY):
            continue
        full = os.path.join(root, path)
        if not os.path.isfile(full):
            continue
        with open(full, "rb") as f:
            data = f.read()
        try:
            text = data.decode("utf-8")
        except UnicodeDecodeError:
            found.append((path, 0, "not valid UTF-8, so it can't be checked"))
            continue
        found += [(path, n, _HINT) for n in hits_in(text)]
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
        "<p>a &mdash; b</p>\n<p>&#8212;</p>\n<p>&#X2014;</p>\n": [1, 2, 3],
        "<p>&#8212 &#08212;</p>\n<p>&#151;</p>\n<p>&#x97;</p>\n": [1, 2, 3],
        "&ndash; and &amp;mdash are fine\n": [],
        "&#1512; &#82120; &#x972; &#x20140; are other characters\n": [],
        "": [],
    }
    for text, want in samples.items():
        got = hits_in(text)
        if got != want:
            failures.append(f"scan of {text!r}: expected {want}, got {got}")
    files = {
        "README.md": ("ok\n" + EM_DASH + "\n").encode("utf-8"),
        "site/index.html": b"<p>&mdash;</p>\n",
        "site/icon.png": b"\x89PNG\xff\xfe",
        "docs/LATIN1.md": "caf\u00e9\n".encode("latin-1"),
        "docs/FINE.md": b"fine\n",
        "core/README.md": EM_DASH.encode("utf-8"),
    }
    want = [("README.md", 2), ("site/index.html", 1), ("docs/LATIN1.md", 0)]
    with tempfile.TemporaryDirectory() as root:
        for path, data in files.items():
            os.makedirs(os.path.join(root, os.path.dirname(path)), exist_ok=True)
            with open(os.path.join(root, path), "wb") as f:
                f.write(data)
        got = [(path, n) for path, n, _ in check(root, sorted(files) + ["gone.md"])]
    if sorted(got) != sorted(want):
        failures.append(f"check of sample files: expected {want}, got {got}")
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    cases = len(scope) + len(samples) + 1
    print(f"self-test: {cases} cases, {len(failures)} failure(s)")
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
    for path, line, reason in found:
        print(f"{path}:{line}: {reason}" if line else f"{path}: {reason}")
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
