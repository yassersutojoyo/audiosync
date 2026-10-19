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
A single-label host (`name.local`) is a leak unless it's one of the generic
examples in `_EXAMPLE_HOSTS`; mDNS service names such as `_airplay._tcp.local`
pass. Files are read as UTF-8, or as UTF-16 when they start with its byte
order mark or show its NUL-every-other-byte pattern; other files with NUL
bytes are skipped as binary.
Outside CI the machine's own hostname is checked too. This file itself isn't
scanned (its patterns would match). Exit status 1 lists each hit as
`path:line: reason`.
"""

import codecs
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

# Single-label names that may stand in front of `.local`: the generic speaker
# hosts used in tests and code reading a field named `local`. Any other bare
# `<label>.local`, `<label>.lan` or `<label>.home.arpa` counts as a real host.
# The label must start the name, so mDNS service names (`_airplay._tcp.local`),
# placeholders (`<name>.local`, `$HOST.local`) and member chains
# (`self.local.addr`) pass.
_EXAMPLE_HOSTS = ("c", "den", "kitchen", "living", "livingroom", "loopback", "model", "speaker")

# Characters that never occur in text: bytes that decode to them as UTF-16 are
# binary after all.
_BINARY_CHARS = re.compile(r"[\x00-\x08\x0e-\x1a\x1c-\x1f]")

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
    (
        "LAN hostname (single label)",
        re.compile(
            r"(?<![\w.$-])(?!(?:" + "|".join(_EXAMPLE_HOSTS) + r")\.)[A-Za-z0-9]+"
            r"\.(?:local|lan|home\.arpa)(?!\.?[\w-])",
            re.IGNORECASE,
        ),
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
            r"(?<![A-Za-z0-9_])" + re.escape(e) + ("" if e.endswith("/") else r"(?![A-Za-z0-9_])"),
            re.IGNORECASE,
        )
        for e in entries
        if e
    ]


def text_of(data):
    """The text in a file's bytes, or None when they look binary.

    UTF-8 is the default and its byte order mark is dropped. UTF-16 is
    recognized by its byte order mark or, without one, by mostly-ASCII text
    leaving a NUL in every other byte. Anything else with a NUL is binary.
    """
    if data.startswith(codecs.BOM_UTF8):
        return data[len(codecs.BOM_UTF8):].decode("utf-8", "replace")
    head = data[:8192]
    if data.startswith(codecs.BOM_UTF16_LE):
        encoding, data = "utf-16-le", data[len(codecs.BOM_UTF16_LE):]
    elif data.startswith(codecs.BOM_UTF16_BE):
        encoding, data = "utf-16-be", data[len(codecs.BOM_UTF16_BE):]
    elif b"\0" not in head:
        return data.decode("utf-8", "replace")
    else:
        # Most high bytes of mostly-ASCII UTF-16 are NUL, and hardly any low
        # byte is (U+0100, U+4E00 and the like have one).
        even, odd = head[0::2].count(0), head[1::2].count(0)
        if odd > len(head) // 4 and even * 16 <= odd:
            encoding = "utf-16-le"
        elif even > len(head) // 4 and odd * 16 <= even:
            encoding = "utf-16-be"
        else:
            return None
    try:
        text = data.decode(encoding)
    except UnicodeDecodeError:
        return None
    return None if _BINARY_CHARS.search(text) else text


def check_file(root, path, names, hostname, explicit):
    try:
        with open(os.path.join(root, path), "rb") as handle:
            data = handle.read()
    except OSError:
        return [f"{path}: can't be read"] if explicit else []
    text = text_of(data)
    if text is None:
        return []
    hits = []
    for number, line in enumerate(text.splitlines(), 1):
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
        "barehost.md": "Built on name.local yesterday.",
        "barehost-dot.md": "Reached (name.local.)",
        "barelan.md": "The share is on nas.lan now.",
        "hostdomain.md": "Reached box.localdomain.",
        "utf16le-bom.txt": codecs.BOM_UTF16_LE + "See /Users/someone/Library/Logs.\n".encode("utf-16-le"),
        "utf16be-bom.txt": codecs.BOM_UTF16_BE + "Built on name.local.\n".encode("utf-16-be"),
        "utf16le.txt": "Logs are in C:\\Users\\someone\\AppData.\r\n".encode("utf-16-le"),
        "utf16be.txt": "Try 10.1.2.3 then.\n".encode("utf-16-be"),
        "utf16le-cjk.txt": "一 Logs: /Users/someone/Library/Logs.\n".encode("utf-16-le"),
        "utf8-bom.md": codecs.BOM_UTF8 + "Built on name.local yesterday.\n".encode(),
        "winhost.md": "Runner DESKTOP-AB12CD3 passed.",
        "planid.md": "Done for Plan A1.",
        "name.md": f"Ask {marker.capitalize()} about it.",
        "folder.md": "The plan is in zebrafolder/STATUS.md.",
        "hyphen.md": f"Run the {marker}-reviewer agent.",
    }
    clean = (
        "Logs: C:\\Users\\<you>\\AppData, /Users/<you>/Library, %USERPROFILE%, "
        "/Users/Shared, C:\\Users\\Public\\Music, 127.0.0.1, Windows 10.0.26200.1, "
        "version 1.10.2.3.4, model.local = [], _raop._tcp.local, the plan of record, "
        "_airplay._tcp.local., Den._raop._tcp.local., LivingRoom.local., map(|c| c.local), "
        "self.local.addr, $HOST.local, <name>.local, Bundle.localized, "
        "a feed: 0xfd12, zebracorns.\n"
    )
    # Binary files are skipped even when they carry a leak, including bytes
    # that only look like UTF-16 until they decode to control characters.
    binary = {
        "image.png": b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR /Users/someone/x \x00\x01",
        "words.bin": b"\x01\x00\x02\x00" + "/Users/someone/x".encode("utf-16-le"),
    }
    samples = {
        **leaks,
        **binary,
        "clean.md": clean,
        "clean16.txt": clean.encode("utf-16"),
        "clean16be.txt": clean.encode("utf-16-be"),
    }
    failures = []
    with tempfile.TemporaryDirectory() as root:
        git(root, "init", "-q")
        for name, content in samples.items():
            if isinstance(content, str):
                content = (content if content.endswith("\n") else content + "\n").encode()
            with open(os.path.join(root, name), "wb") as handle:
                handle.write(content)
        listing = os.path.join(root, "names.txt")
        with open(listing, "w", encoding="utf-8") as handle:
            handle.write("# sample list\n" + marker + "\nzebrafolder/\n")
        names = name_patterns(listing)
        git(root, "add", "--", *samples)
        files = git(root, "ls-files").split()
        for name in leaks:
            if name not in files or not check(root, [name], names):
                failures.append(f"not caught: {name}")
        for name in ("clean.md", "clean16.txt", "clean16be.txt"):
            failures += [f"false positive: {hit}" for hit in check(root, [name], names)]
        for name in binary:
            failures += [f"binary file scanned: {hit}" for hit in check(root, [name], names)]
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
