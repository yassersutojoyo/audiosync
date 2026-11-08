#!/usr/bin/env python3
"""Fails when public documentation contains an em dash (U+2014).

HTML entities for it (&mdash;, &#8212;, &#x2014;) count too, and a text file
in scope that isn't valid UTF-8 fails rather than being skipped unread.

Scope is the tracked text people outside the project read: every Markdown
file, everything under site/ and remote/page/, and the notices the Linux
AppImage ships. docs/*_SPEC.md is left out because the specs quote test output
verbatim. App strings, code and comments are out of scope.

A full run also fails when an inline script a page under site/ runs has a
sha256 that vercel.json's Content-Security-Policy doesn't list in script-src,
since the browser would block it.

Usage:
    scripts/text_check.py              check every tracked file in scope
    scripts/text_check.py FILE...      check the given files (scope still applies)
    scripts/text_check.py --self-test  check the scope rules and the scanner
"""

import argparse
import base64
import hashlib
import json
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


# Left out: desktop/src/windows_strings.txt (app text, tested in desktop/src/contrast.rs)
# and the license texts kept verbatim (brand/fonts/OFL.txt, desktop/linux/RUNTIME-NOTICES.txt).
def in_scope(path):
    if path.startswith(("site/", "remote/page/")):
        return True
    return path.endswith(".md") and not _SPEC.fullmatch(path)


def hits_in(text):
    return [
        n
        for n, line in enumerate(text.splitlines(), 1)
        if EM_DASH in line or _ENTITY.search(line)
    ]


_SCRIPT = re.compile(r"<script\b([^>]*)>(.*?)</script\s*>", re.IGNORECASE | re.DOTALL)
_SCRIPT_TYPE = re.compile(r"""\btype\s*=\s*["']?([^"'\s>]*)""", re.IGNORECASE)
# A script of any other type, such as application/ld+json, is a data block the browser never runs.
_RUN_TYPES = {"", "text/javascript", "application/javascript", "module"}


def csp_misses(pages, csp):
    """(path, line) of each inline script in `pages` whose sha256 the CSP's script-src lacks."""
    allowed = set()
    for directive in (csp or "").split(";"):
        words = directive.split()
        if words and words[0].lower() == "script-src":
            allowed.update(words[1:])
    misses = []
    for path, html in sorted(pages.items()):
        for match in _SCRIPT.finditer(html):
            attrs, body = match.groups()
            kind = _SCRIPT_TYPE.search(attrs)
            if re.search(r"\bsrc\s*=", attrs, re.IGNORECASE) or (kind.group(1).lower() if kind else "") not in _RUN_TYPES:
                continue
            digest = base64.b64encode(hashlib.sha256(body.encode("utf-8")).digest()).decode()
            if f"'sha256-{digest}'" not in allowed:
                misses.append((path, html.count("\n", 0, match.start()) + 1))
    return misses


def site_csp_misses(root, paths):
    """The site's inline scripts that vercel.json's Content-Security-Policy would block."""
    with open(os.path.join(root, "vercel.json"), encoding="utf-8") as f:
        config = json.load(f)
    csp = " ; ".join(
        header["value"]
        for rule in config.get("headers", [])
        for header in rule.get("headers", [])
        if header.get("key", "").lower() == "content-security-policy"
    )
    pages = {}
    for path in paths:
        if path.startswith("site/") and path.endswith(".html"):
            with open(os.path.join(root, path), encoding="utf-8") as f:
                pages[path] = f.read()
    return csp_misses(pages, csp)


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
        "desktop/README.md": True,
        "docs/DEVELOPMENT.md": True,
        "docs/ENGINE_SPEC.md": False,
        "docs/AIRPLAY_SPEC.md": False,
        "docs/sub/NOTES.md": True,
        "site/index.html": True,
        "site/icons/manifest.json": True,
        "core/README.md": True,
        "brand/README.md": True,
        "remote/page/index.html": True,
        "desktop/linux/RUNTIME-NOTICES.txt": False,
        "desktop/src/windows_strings.txt": False,
        "brand/fonts/OFL.txt": False,
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
        "core/src/lib.rs": EM_DASH.encode("utf-8"),
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
    body = "\nconsole.log(1);\n"
    good = "'sha256-" + base64.b64encode(hashlib.sha256(body.encode("utf-8")).digest()).decode() + "'"
    script = "<script>" + body + "</script>\n"
    csp_cases = {
        # The inline script edited after vercel.json took its hash.
        "edited script": ({"site/index.html": "<p>x</p>\n" + script.replace("1", "2")},
                          "default-src 'none'; script-src " + good, [("site/index.html", 2)]),
        "matching hash": ({"site/index.html": script},
                          "default-src 'none'; script-src " + good + "; style-src 'unsafe-inline'", []),
        "hash in another directive": ({"site/a.html": script}, "style-src " + good, [("site/a.html", 1)]),
        "ld+json data block": ({"site/a.html": '<script type="application/ld+json">{}</script>'}, "script-src 'none'", []),
        "external script": ({"site/a.html": '<script src="/a.js"></script>'}, "script-src 'self'", []),
        "module script": ({"site/a.html": '<script type="module">' + body + "</script>"}, "script-src 'self'",
                          [("site/a.html", 1)]),
    }
    for name, (pages, csp, want) in csp_cases.items():
        got = csp_misses(pages, csp)
        if got != want:
            failures.append(f"CSP check, {name}: expected {want}, got {got}")
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    cases = len(scope) + len(samples) + len(csp_cases) + 1
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
    if not args.files:
        found += [(path, line, "inline script whose sha256 vercel.json's script-src doesn't list")
                  for path, line in site_csp_misses(root, paths)]
    for path, line, reason in found:
        print(f"{path}:{line}: {reason}" if line else f"{path}: {reason}")
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
