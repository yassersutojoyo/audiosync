#!/usr/bin/env python3
"""Fails when a tracked file carries a private reference.

This repository is public. Published files must not contain home-directory
paths from either development machine (macOS `/Users/...`, Windows
`C:\\Users\\...` or `C:/Users/...`, Git Bash `/c/Users/...`), LAN hostnames,
private or link-local IP addresses, or internal plan IDs.

Names that must stay private can't be listed in a public script without
publishing them, so they live in an untracked, one-per-line list:
`$AUDIOSYNC_PRIVATE_NAMES`, or else `info/private-names` in the repository's
git directory (shared by every worktree; `#` starts a comment). Each entry is
matched case-insensitively as a whole word. Without the list (in CI, for
example) that rule is skipped and the script says so.

    scripts/privacy_check.py              check every tracked file
    scripts/privacy_check.py FILE...      check only these files
    scripts/privacy_check.py --self-test  seed a scratch repository with one
                                          leak per rule and expect each to be
                                          caught

Placeholders such as `C:\\Users\\<you>`, `/Users/%USER%` or `/Users/Shared` pass.
Outside CI the machine's own hostname is checked too. This file itself isn't
scanned (its patterns would match). Exit status 1 lists each hit as
`path:line: reason`.
"""

import os
import re
import socket
import subprocess
import sys
import tempfile

# What may follow a home directory without being a leak: a placeholder, an
# environment variable, or a shared profile.
_PLACEHOLDER = r"(?![<%{$]|(?:Shared|Public|Default)(?![\w.-]))"
_SEP = r"(?:\\{1,4}|/)"

PATTERNS = [
    ("home path (/Users/...)", re.compile(r"/Users/" + _PLACEHOLDER + r"[^\s/\"'`)\]]+")),
    (
        "Windows home path",
        re.compile(
            r"(?:\b[A-Za-z]:|%SystemDrive%)" + _SEP + "Users" + _SEP + _PLACEHOLDER
            + r"[^\s\\/\"'`)\]]+",
            re.IGNORECASE,
        ),
    ),
    (
        "private or link-local IPv4 address",
        re.compile(
            r"(?<![\d.])(?:10\.\d{1,3}|172\.(?:1[6-9]|2\d|3[01])|192\.168|169\.254"
            r"|100\.(?:6[4-9]|[7-9]\d|1[01]\d|12[0-7]))\.\d{1,3}\.\d{1,3}(?!\.?\d)"
        ),
    ),
    (
        "unique local IPv6 address",
        re.compile(r"(?<![\w:])fd[0-9a-f]{2}(?::[0-9a-f]{0,4}){2,7}(?![\w:])", re.IGNORECASE),
    ),
    (
        "LAN hostname",
        re.compile(r"\b[A-Za-z0-9]+(?:-[A-Za-z0-9]+)+\.(?:local|lan|home\.arpa)\b|\b\w+\.localdomain\b"),
    ),
    ("default Windows hostname", re.compile(r"\b(?:DESKTOP|LAPTOP)-[A-Z0-9]{7}\b")),
    ("internal plan ID", re.compile(r"\bPLAN (?:v\d+|[A-F]\d{1,2})\b", re.IGNORECASE)),
]

SELF = "scripts/privacy_check.py"


def git(root, *args):
    return subprocess.run(
        ["git", "-C", root, *args], check=True, capture_output=True, text=True
    ).stdout


def name_patterns(path):
    """Whole-word patterns for each entry of the private list at `path`."""
    try:
        with open(path, encoding="utf-8") as handle:
            entries = [line.split("#", 1)[0].strip() for line in handle]
    except OSError:
        return None
    # A folder entry ("name/") matches with anything after it, so a path
    # inside the folder is caught; other entries must end at a word boundary.
    return [
        re.compile(
            r"(?<![A-Za-z0-9_])" + re.escape(e) + ("" if e.endswith("/") else r"(?![A-Za-z0-9_-])"),
            re.IGNORECASE,
        )
        for e in entries
        if e
    ]


def check_file(root, path, names, hostname, explicit):
    try:
        with open(os.path.join(root, path), "rb") as handle:
            data = handle.read()
    except OSError:
        return [f"{path}: can't be read"] if explicit else []
    if b"\0" in data[:8192]:
        return []
    hits = []
    for number, line in enumerate(data.decode("utf-8", "replace").splitlines(), 1):
        for reason, pattern in PATTERNS:
            if pattern.search(line):
                hits.append(f"{path}:{number}: {reason}")
        if any(p.search(line) for p in names):
            hits.append(f"{path}:{number}: private name")
        if hostname and re.search(r"(?<![\w-])" + re.escape(hostname) + r"(?![\w-])", line, re.I):
            hits.append(f"{path}:{number}: this machine's hostname")
    return hits


def check(root, paths, names=(), hostname=None, explicit=False):
    hits = []
    for path in paths:
        if path.replace("\\", "/") != SELF:
            hits.extend(check_file(root, path, names, hostname, explicit))
    return hits


def self_test():
    marker = "zebracorn-sample"
    leaks = {
        "win-backslash.md": "Logs are in C:\\Users\\someone\\AppData\\Local\\Temp.",
        "win-json.txt": 'path = "C:\\\\Users\\\\someone\\\\x"',
        "win-slash.md": "Open c:/Users/someone/Projects/x first.",
        "win-env.md": "Look in %SystemDrive%\\Users\\someone.",
        "gitbash.sh": "cd /c/Users/someone/Projects",
        "mac.md": "See /Users/someone/Library/Logs.",
        "ip.md": "The receiver answered from 192.168.1.20.",
        "ip10.md": "Try 10.1.2.3 then.",
        "ipcgnat.md": "The tunnel uses 100.101.2.3.",
        "iplink.md": "It fell back to 169.254.10.20.",
        "ipv6.md": "Listening on fd12:3456:789a::1 now.",
        "host.md": "Built on Someones-MacBook-Pro.local yesterday.",
        "hostdomain.md": "Reached box.localdomain.",
        "winhost.md": "Runner DESKTOP-AB12CD3 passed.",
        "planid.md": "Done for Plan A1.",
        "name.md": f"Ask {marker.capitalize()} about it.",
        "folder.md": "The plan is in zebrafolder/STATUS.md.",
    }
    clean = (
        "Logs: C:\\Users\\<you>\\AppData, /Users/<you>/Library, %USERPROFILE%, "
        "/Users/Shared, C:\\Users\\Public\\Music, 127.0.0.1, Windows 10.0.26200.1, "
        "version 1.10.2.3.4, model.local = [], _raop._tcp.local, the plan of record, "
        "a feed: 0xfd12, zebracorns.\n"
    )
    failures = []
    with tempfile.TemporaryDirectory() as root:
        git(root, "init", "-q")
        for name, text in leaks.items():
            with open(os.path.join(root, name), "w", encoding="utf-8") as handle:
                handle.write(text + "\n")
        with open(os.path.join(root, "clean.md"), "w", encoding="utf-8") as handle:
            handle.write(clean)
        listing = os.path.join(root, "names.txt")
        with open(listing, "w", encoding="utf-8") as handle:
            handle.write("# sample list\n" + marker + "\nzebrafolder/\n")
        names = name_patterns(listing)
        git(root, "add", "--", *leaks, "clean.md")
        files = git(root, "ls-files").split()
        for name in leaks:
            if name not in files or not check(root, [name], names):
                failures.append(f"not caught: {name}")
        failures += [f"false positive: {hit}" for hit in check(root, ["clean.md"], names)]
        if not check(root, ["missing.md"], names, explicit=True):
            failures.append("a missing file argument passed")
    for failure in failures:
        print(failure, file=sys.stderr)
    print(f"self-test: {len(leaks)} leaks, {len(failures)} failure(s)")
    return 1 if failures else 0


def main(argv):
    if argv[:1] == ["--self-test"]:
        return self_test()
    root = git(".", "rev-parse", "--show-toplevel").strip()
    if argv:
        paths = [os.path.relpath(os.path.abspath(p), root).replace("\\", "/") for p in argv]
    else:
        paths = git(root, "ls-files", "-z").split("\0")[:-1]
    listing = os.environ.get("AUDIOSYNC_PRIVATE_NAMES") or os.path.join(
        git(root, "rev-parse", "--path-format=absolute", "--git-common-dir").strip(),
        "info",
        "private-names",
    )
    names = name_patterns(listing)
    if names is None:
        print("private names: no local list, skipped")
    hostname = None
    if not os.environ.get("CI"):
        name = socket.gethostname().split(".")[0]
        hostname = name if len(name) >= 3 and name.lower() != "localhost" else None
    hits = check(root, paths, names or (), hostname, explicit=bool(argv))
    for hit in hits:
        print(hit)
    if hits:
        print(f"{len(hits)} private reference(s); see the docstring of {SELF}", file=sys.stderr)
        return 1
    print(f"private references: none in {len(paths)} file(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
