#!/usr/bin/env python3
"""Fails when a tracked file carries a private reference.

This repository is public. Published files must not contain home-directory
paths from either development machine (macOS `/Users/...`, Windows
`C:\\Users\\...` or `C:/Users/...`, Git Bash `/c/Users/...`), LAN hostnames,
private or link-local IP addresses, internal plan IDs, or numbered entries
from the private complaints ledger.

Names that must stay private can't be listed in a public script without
publishing them, so they live in an untracked, one-per-line list:
`$AUDIOSYNC_PRIVATE_NAMES`, or else `info/private-names` in the repository's
git directory (shared by every worktree; `#` starts a comment). Each entry is
matched case-insensitively as a whole word. Without the list (in CI, for
example) that rule is skipped and the script says so.

Forbidden names (names this repository must never spell, such as the projects the
clean-room specs studied) can't be listed in plain text either, so
`scripts/forbidden-names.txt` keeps a salt and, per name, the SHA-256 of the
salt plus the normalized name (ASCII letters and digits, lowercased). Text is
split into words at CamelCase humps and at joiners: runs of spaces, tabs,
no-break spaces, hyphens (ASCII, U+2010, U+2011) and underscores, or a single
dot, plus, slash or `%20`. Every run of one to four adjacent words is joined,
lowercased and hashed, also with a glued `lib` or `my` in front or an `s` or
`d` at the end taken off, so "Foo Bar", "foo-bar", "foo.bar", "FooBar",
"FOO_BAR", "foobar", "libfoobar" and "foobars" all hit. Any other character
(a newline, a comma, a dot followed by a space) ends a run, and a name inside
a longer lowercase word ("foobarbaz") isn't caught. A name flagged `title`
counts in its spaced spelling only in Title Case ("Foo Bar", not "foo bar").
The rule covers tracked file contents and paths and, with `--commits`, commit
messages, where a line break inside a paragraph reads as a space so a wrapped
name still hits. A hit names the place, never the matched text, so CI logs
don't spell the name either.

The hashes keep the names out of the published files and out of a grep;
they don't hide them from anyone who tries: the salt is public, so a short
name falls to a brute-force search in minutes, and any name falls to a
guess. That's acceptable, since the names are public project names and the
goal is only that this repository never spells them. `allow` and
`allow-commit` lines in that file waive hits by path and line pattern (see
its header). The plain list lives only in an untracked file:
`$AUDIOSYNC_FORBIDDEN_NAMES`, else `info/forbidden-names` in the worktree's
git directory, else in the repository's common one; a trailing `[title]`
on an entry sets that flag.

    scripts/privacy_check.py              check every tracked file
    scripts/privacy_check.py FILE...      check only these files
    scripts/privacy_check.py --self-test  seed a scratch repository with one
                                          leak per rule and expect each to be
                                          caught
    --commits RANGE                       check the messages of the commits in
                                          RANGE (`A..B`); an all-zero or unknown
                                          A, as on a new branch's first push,
                                          checks `origin/main..B`, or B alone
                                          without origin/main
    --hash-names [PATH]                   rewrite scripts/forbidden-names.txt
                                          from the plain list at PATH (default:
                                          the local list), keeping its salt and
                                          allow lines
    --verify-names                        with a local plain list, check that
                                          the hash file matches it and that
                                          each name's usual spellings are
                                          caught; without one, skip

Placeholders such as `C:\\Users\\<you>`, `/Users/%USER%` or `/Users/Shared` pass.
A single-label host (`name.local`) is a leak unless it's one of the generic
examples in `_EXAMPLE_HOSTS`; mDNS service names such as `_airplay._tcp.local`
pass. Files are read as UTF-8, or as UTF-16 when they start with its byte
order mark or show its NUL-every-other-byte pattern; other files with NUL
bytes are skipped as binary.
Outside CI the machine's own hostname is checked too. This file itself is
scanned only for forbidden names (its other patterns would match). Exit
status 1 lists each hit as `path:line: reason`.
"""

import argparse
import codecs
import fnmatch
import hashlib
import os
import re
import secrets
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
    ("private ledger reference", re.compile(r"\b(?:complaint|keluhan) #?\d+\b", re.IGNORECASE)),
]

SELF = "scripts/privacy_check.py"


def git(root, *args):
    return subprocess.run(
        ["git", "-C", root, *args], check=True, capture_output=True, text=True
    ).stdout


# Forbidden names, stored as salted hashes (see the docstring).
NAMES_FILE = "scripts/forbidden-names.txt"
NAMES_HEADER = """\
# Forbidden names for scripts/privacy_check.py: the salt, then one hash per
# name, SHA-256 of the salt plus the name's ASCII letters and digits in lower
# case, so the names never appear in a published file. Rewrite the hashes
# from the untracked plain list with `scripts/privacy_check.py --hash-names`;
# the salt and the allow lines are kept, and the flags come from the list.
#
# HASH title
#   a name whose spaced spelling ("Foo Bar", also with a no-break space or
#   %20) counts only in Title Case, for a name that reads as ordinary words in
#   lower case; its glued, CamelCase, hyphenated and other spellings still
#   count. The plain list marks it with a trailing [title].
# allow GLOB [REGEX]
#   waives forbidden-name hits in tracked files whose repository-relative path
#   matches GLOB (fnmatch): on lines where REGEX (Python re.search) matches,
#   or, without a REGEX, everywhere in the file and in its path.
# allow-commit REGEX
#   waives forbidden-name hits in commit messages on lines where REGEX
#   matches (the line the name starts on).
# A rule must never spell a forbidden name.
"""
# The longest run of adjacent words tried as one name ("Foo Bar Baz Qux").
MAX_RUN = 4
# Shorter names would hit ordinary words.
MIN_NAME = 3
# Words: CamelCase humps, all-caps runs and digit runs, so "FooBar", "FOOBar"
# and "foo2" split where a reader would.
_WORD = re.compile(r"[A-Z]+(?![a-z])|[A-Z]?[a-z]+|[0-9]+")
# A chain's tokens: a URL-encoded space (a joiner, not the word "20") or a word.
_TOKEN = re.compile(r"%20|" + _WORD.pattern)
# Words separated by nothing or by joiners form one chain; any other character
# ends it. Joiners: runs of spaces, tabs, no-break spaces, hyphens (ASCII,
# U+2010, U+2011) and underscores, or one dot, plus, slash or `%20` run, so
# "foo.bar" and "foo/bar" join but a sentence's "foo. Bar" doesn't.
_SPACING = "[ \t _‐‑-]+"
_CHAIN = re.compile(rf"[A-Za-z0-9]+(?:(?:{_SPACING}|[./+]|(?:%20)+)[A-Za-z0-9]+)*")
# The joiners that make a spaced spelling ("Foo Bar"), which a name flagged
# `title` only counts in Title Case.
_SPACED = re.compile(r"[ \t ]|%20")
# Affixes glued to a name in lower case ("libfoo", "myfoo", "foos", "food"):
# each run is also hashed with one of these removed from either end.
_PREFIXES = ("", "lib", "my")
_SUFFIXES = ("", "s", "d")
TITLE = "title"
_HASH = re.compile(r"[0-9a-f]{64}")
_ZERO_SHA = re.compile(r"0+")
# A plain-list entry: the name, then an optional flag in brackets.
_PLAIN_ENTRY = re.compile(r"(.*?)\s*(?:\[\s*([^\]]*?)\s*\])?")


def normalize(name):
    """A name's letters and digits in lower case: "Foo-Bar" -> "foobar"."""
    return "".join(_WORD.findall(name)).lower()


class ForbiddenNames:
    """The salted name hashes, flags and allow rules of a forbidden-names file."""

    def __init__(self, salt, hashes, allow=(), title=(), allow_commit=()):
        self.salt = salt
        self.hashes = frozenset(bytes.fromhex(h) for h in hashes)
        # Names whose spaced spelling ("Foo Bar") only counts in Title Case.
        self.title = frozenset(bytes.fromhex(h) for h in title)
        self.allow = list(allow)
        self.allow_commit = list(allow_commit)
        self._salt = salt.encode()
        self._chains = {}

    @classmethod
    def load(cls, path):
        salt, hashes, title, allow, allow_commit = None, [], [], [], []
        with open(path, encoding="utf-8") as handle:
            for number, raw in enumerate(handle, 1):
                line = raw.strip()
                if not line or line.startswith("#"):
                    continue
                key, _, rest = line.partition(" ")
                rest = rest.strip()
                try:
                    if key == "salt" and rest and salt is None:
                        salt = rest
                    elif key == "allow" and rest:
                        glob, _, regex = rest.partition(" ")
                        regex = regex.strip()
                        allow.append((glob, re.compile(regex) if regex else None))
                    elif key == "allow-commit" and rest:
                        allow_commit.append(re.compile(rest))
                    elif _HASH.fullmatch(key) and rest in ("", TITLE):
                        hashes.append(key)
                        if rest:
                            title.append(key)
                    else:
                        raise ValueError(
                            f"{path}:{number}: not a salt, allow, allow-commit or hash line"
                        )
                except re.error as error:
                    where = "" if error.pos is None else f" at position {error.pos}"
                    raise ValueError(f"{path}:{number}: bad pattern: {error.msg}{where}") from None
        if salt is None:
            raise ValueError(f"{path}: no salt line")
        return cls(salt, hashes, allow, title, allow_commit)

    def digest(self, normalized):
        return hashlib.sha256(self._salt + normalized.encode()).digest()

    def _name_digest(self, run):
        """The digest of the name `run` spells, bare or with a glued affix removed, or None."""
        hashes = self.hashes
        for prefix in _PREFIXES:
            if not run.startswith(prefix):
                continue
            for suffix in _SUFFIXES:
                core = run[len(prefix):len(run) - len(suffix)]
                if len(core) >= MIN_NAME and run.endswith(suffix):
                    digest = self.digest(core)
                    if digest in hashes:
                        return digest
        return None

    def _chain_spans(self, chain):
        """(start, end) in `chain` of each run of words that spells a name."""
        words = [m for m in _TOKEN.finditer(chain) if m.group() != "%20"]
        lower = [m.group().lower() for m in words]
        # spaced[k]: word k follows a space-like joiner, not a hyphen or a dot.
        spaced = [False] + [
            bool(_SPACED.search(chain, a.end(), b.start())) for a, b in zip(words, words[1:])
        ]
        found = []
        for i in range(len(words)):
            run, best = "", None
            for j in range(i, min(i + MAX_RUN, len(words))):
                run += lower[j]
                digest = self._name_digest(run)
                if digest is None:
                    continue
                if digest in self.title and any(spaced[i + 1:j + 1]) and not all(
                    w.group()[0].isupper() or w.group()[0].isdigit() for w in words[i:j + 1]
                ):
                    continue
                best = (words[i].start(), words[j].end())
            if best:
                found.append(best)
        merged = []
        for start, end in found:
            if merged and start <= merged[-1][1]:
                merged[-1] = (merged[-1][0], max(merged[-1][1], end))
            else:
                merged.append((start, end))
        return tuple(merged)

    def spans(self, text):
        """(start, end) of each run of words in `text` that spells a name."""
        found = []
        chains = self._chains
        for match in _CHAIN.finditer(text):
            chain = match.group()
            hits = chains.get(chain)
            if hits is None:
                hits = chains[chain] = self._chain_spans(chain)
            base = match.start()
            found += [(base + start, base + end) for start, end in hits]
        return found

    def mask(self, text):
        """`text` with each run of words that spells a name replaced by `***`."""
        parts, last = [], 0
        for start, end in self.spans(text):
            parts += [text[last:start], "***"]
            last = end
        return "".join(parts) + text[last:]

    def allowed(self, path, line=None):
        """Whether an allow rule waives a hit in `path` (on `line`, or in the path)."""
        for glob, regex in self.allow:
            if fnmatch.fnmatchcase(path, glob) and (
                regex is None or (line is not None and regex.search(line))
            ):
                return True
        return False

    def commit_allowed(self, line):
        """Whether an allow-commit rule waives a hit on this commit-message line."""
        return any(regex.search(line) for regex in self.allow_commit)

    def file_hits(self, path, text):
        """Hits in a file's path (shown masked) and in its text, by line number."""
        hits, shown = [], self.mask(path)
        if shown != path and not self.allowed(path):
            hits.append(f"{shown}: forbidden name in the path")
        lines = set()
        for offset, _ in self.spans(text):
            start = text.rfind("\n", 0, offset) + 1
            end = text.find("\n", offset)
            line = text[start:end if end >= 0 else len(text)].rstrip("\r")
            if not self.allowed(path, line):
                lines.add(text.count("\n", 0, offset) + 1)
        hits += [f"{shown}:{number}: forbidden name" for number in sorted(lines)]
        return hits


def split_entry(entry):
    """A plain-list entry's name and flag (None without one): "Foo Bar [title]"."""
    name, flag = _PLAIN_ENTRY.fullmatch(entry).groups()
    return name, flag


def read_plain_names(path):
    """(line number, entry) for each name in a plain list; `#` starts a comment."""
    with open(path, encoding="utf-8") as handle:
        entries = [(n, line.split("#", 1)[0].strip()) for n, line in enumerate(handle, 1)]
    return [(n, e) for n, e in entries if e]


def name_problem(entry):
    """Why a plain-list entry can't be hashed, or None. Never quotes the entry."""
    name, flag = split_entry(entry)
    if flag is not None and flag != TITLE:
        return f"unknown flag; the only one is [{TITLE}]"
    if not re.fullmatch(r"[A-Za-z0-9]+(?:[ _-]+[A-Za-z0-9]+)*", name):
        return "only ASCII letters and digits, joined by spaces, hyphens or underscores"
    if len(normalize(name)) < MIN_NAME:
        return f"shorter than {MIN_NAME} letters and digits"
    if len(_WORD.findall(name)) > MAX_RUN:
        return f"more than {MAX_RUN} words"
    return None


def write_names_file(plain, out, salt=None):
    """Hash the plain list at `plain` into `out`; returns the problems found.

    The salt and allow lines of an existing `out` are kept unless `salt` is
    given; a new file gets a random salt.
    """
    entries = read_plain_names(plain)
    problems = [
        f"{plain}:{n}: {problem}" for n, e in entries if (problem := name_problem(e))
    ]
    if problems:
        return problems
    allow_lines = []
    if os.path.exists(out):
        with open(out, encoding="utf-8") as handle:
            for raw in handle:
                key, _, rest = raw.strip().partition(" ")
                if key == "salt" and salt is None:
                    salt = rest.strip()
                elif key in ("allow", "allow-commit"):
                    allow_lines.append(raw.strip())
    salt = salt or secrets.token_hex(16)
    names = ForbiddenNames(salt, ())
    flags = {}
    for _, entry in entries:
        name, flag = split_entry(entry)
        digest = names.digest(normalize(name)).hex()
        flags[digest] = flags.get(digest) or flag
    with open(out, "w", encoding="utf-8", newline="\n") as handle:
        handle.write(NAMES_HEADER + f"salt {salt}\n")
        handle.writelines(line + "\n" for line in allow_lines)
        handle.writelines(f"{d} {f}\n" if f else f"{d}\n" for d, f in sorted(flags.items()))
    return []


def spellings(name, title=False):
    """The usual ways to write a name, each of which must be caught.

    As listed, joined, in CamelCase, with each word joiner, and with a glued
    `lib`, `my`, plural `s` or daemon `d`. A `title` name's spaced spellings
    count only in Title Case, so its lower-case ones are left out.
    """
    words = _WORD.findall(name)
    lower = [w.lower() for w in words]
    camel = [w[:1].upper() + w[1:].lower() for w in words]
    glued, hump = "".join(lower), "".join(camel)
    found = {
        name, name.upper(), glued, "".join(words).upper(), hump, hump + "Client",
        hump + "s", "lib" + hump, "lib" + glued, "my" + glued, glued + "s", glued + "d",
        "lib" + glued + "s",
    }
    for joiner in ("-", "_", ".", "+", "/", "‐", "‑"):
        found |= {joiner.join(lower), joiner.join(camel)}
    for joiner in (" ", " ", "%20", " \t "):
        found.add(joiner.join(camel))
        if not title:
            found.add(joiner.join(lower))
    if not title or not _SPACED.search(name):
        found.add(name.lower())
    return found


def verify_names(names, plain):
    """Problems between a hash file and its plain list, by list line number."""
    problems, expected = [], set()
    for number, entry in read_plain_names(plain):
        problem = name_problem(entry)
        if problem:
            problems.append(f"plain list line {number}: {problem}")
            continue
        name, flag = split_entry(entry)
        title = flag == TITLE
        digest = names.digest(normalize(name))
        expected.add(digest)
        if digest not in names.hashes:
            problems.append(f"plain list line {number}: not in the hash file; run --hash-names")
        elif (digest in names.title) != title:
            problems.append(f"plain list line {number}: its flag differs from the hash file; "
                            "run --hash-names")
        elif any(not names.spans(f"Ask {s} today.") for s in spellings(name, title)):
            problems.append(f"plain list line {number}: a usual spelling isn't caught")
        elif title and len(_WORD.findall(name)) > 1 and names.spans(
            "Ask " + " ".join(w.lower() for w in _WORD.findall(name)) + " today."
        ):
            problems.append(f"plain list line {number}: the lower-case spaced spelling of a "
                            f"[{TITLE}] name is caught")
    stale = len(names.hashes - expected)
    if stale:
        problems.append(f"{stale} hash(es) match no name in the plain list; run --hash-names")
    return problems


def plain_list_problem(root, path):
    """Why the plain list at `path` can't or mustn't be read, or None.

    The list must exist and stay out of git: it may live outside the work tree
    (the git directory, for example) or inside it only as an ignored,
    untracked file.
    """
    if not os.path.isfile(path):
        return f"the plain list doesn't exist: {path}"
    try:
        rel = os.path.relpath(os.path.abspath(path), root).replace("\\", "/")
    except ValueError:
        return None  # another drive, so outside the work tree
    if rel == ".." or rel.startswith("../") or rel.split("/")[0] == ".git":
        return None
    tracked = subprocess.run(
        ["git", "-C", root, "ls-files", "--error-unmatch", "--", rel], capture_output=True
    ).returncode == 0
    ignored = subprocess.run(
        ["git", "-C", root, "check-ignore", "-q", "--", rel], capture_output=True
    ).returncode == 0
    if tracked or not ignored:
        return "the plain list must be untracked and ignored, or outside the work tree"
    return None


def local_plain_list(root):
    """The path of the local plain list, or None when there's none."""
    listing = os.environ.get("AUDIOSYNC_FORBIDDEN_NAMES")
    if listing:
        return listing
    for which in ("--git-dir", "--git-common-dir"):
        gitdir = git(root, "rev-parse", "--path-format=absolute", which).strip()
        candidate = os.path.join(gitdir, "info", "forbidden-names")
        if os.path.isfile(candidate):
            return candidate
    return None


def commit_range(root, spec):
    """The `git log` arguments for RANGE, with a new branch's all-zero base handled."""
    if spec.startswith("-"):
        raise ValueError("a commit range can't start with '-'")
    left, dots, right = spec.partition("..")
    if not dots or right.startswith("."):
        return [spec]
    right = right or "HEAD"
    known = left and not _ZERO_SHA.fullmatch(left) and subprocess.run(
        ["git", "-C", root, "rev-parse", "-q", "--verify", left + "^{commit}"], capture_output=True
    ).returncode == 0
    if known:
        return [f"{left}..{right}"]
    has_main = subprocess.run(
        ["git", "-C", root, "rev-parse", "-q", "--verify", "refs/remotes/origin/main^{commit}"],
        capture_output=True,
    ).returncode == 0
    return [f"origin/main..{right}"] if has_main else ["-1", right]


def commit_messages(root, spec):
    """(commit, message) for each commit in RANGE."""
    out = git(root, "log", "-z", "--format=%H%n%B", *commit_range(root, spec), "--")
    messages = []
    for record in out.split("\0"):
        commit, _, message = record.strip("\n").partition("\n")
        if commit:
            messages.append((commit, message))
    return messages


def join_wrapped(message):
    """`message` with each line break inside a paragraph read as a space.

    A name wrapped onto the next line still counts. The length stays the same,
    so an offset in the result is on the same line of the original.
    """
    lines = message.split("\n")
    parts = []
    for number, line in enumerate(lines):
        parts.append(line)
        if number + 1 < len(lines):
            parts.append(" " if line.strip() and lines[number + 1].strip() else "\n")
    return "".join(parts)


def check_commits(root, spec, names):
    hits = []
    for commit, message in commit_messages(root, spec):
        physical = message.split("\n")
        lines = set()
        for offset, _ in names.spans(join_wrapped(message)):
            number = message.count("\n", 0, offset) + 1
            if not names.commit_allowed(physical[number - 1]):
                lines.add(number)
        hits += [f"commit {commit[:12]}:{n}: forbidden name" for n in sorted(lines)]
    return hits


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


def check_file(root, path, names, hostname, explicit, forbidden=None, patterns=True):
    try:
        with open(os.path.join(root, path), "rb") as handle:
            data = handle.read()
    except OSError:
        return [f"{path}: can't be read"] if explicit else []
    text = text_of(data)
    hits, shown = [], path
    if forbidden is not None:
        # The path is shown masked in every hit, so no report spells a name.
        hits, shown = forbidden.file_hits(path, "" if text is None else text), forbidden.mask(path)
    if text is None or not patterns:
        return hits
    for number, line in enumerate(text.splitlines(), 1):
        for reason, pattern in PATTERNS:
            if pattern.search(line):
                hits.append(f"{shown}:{number}: {reason}")
        if any(p.search(line) for p in names):
            hits.append(f"{shown}:{number}: private name")
        if hostname and re.search(r"(?<![\w-])" + re.escape(hostname) + r"(?![\w-])", line, re.I):
            hits.append(f"{shown}:{number}: this machine's hostname")
    return hits


def check(root, paths, names=(), hostname=None, explicit=False, forbidden=None):
    """Hits in `paths`; `forbidden` (a ForbiddenNames) turns that rule on."""
    hits = []
    for path in paths:
        is_self = path.replace("\\", "/") == SELF
        if not is_self or forbidden is not None:
            hits.extend(check_file(root, path, names, hostname, explicit, forbidden, not is_self))
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
        "ledger.md": "Measured for complaint 34.",
        "ledger-id.md": "See Keluhan #7 first.",
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
    cases, forbidden_failures = forbidden_self_test()
    failures += forbidden_failures
    for failure in failures:
        print(failure, file=sys.stderr)
    print(f"self-test: {len(leaks)} leaks, {cases} forbidden-name cases, {len(failures)} failure(s)")
    return 1 if failures else 0


def forbidden_self_test():
    """The forbidden-name rule, proven on made-up names with their own salt.

    Returns (number of cases, failures). Covers the usual spellings in file
    contents, file paths and commit messages (glued affixes, every joiner, a
    wrapped commit line), near misses, a `title` name, the allow and
    allow-commit rules, a bad allow pattern, masking, a run without the rule,
    the hash file keeping its salt, flags and allow lines, plain-list checks
    and a new branch's all-zero base.
    """
    listed = [
        "Gloam Tavern", "brindlefox", "skarn-vole", "Quill Marrow Fen Dart", "Cinder Ledger [title]",
    ]
    variants = [
        "Ask Gloam Tavern first.", "gloam tavern", "gloam-tavern", "GloamTavern",
        "GLOAM_TAVERN", "gloamtavern", "Gloam \t Tavern", "the GloamTavernClient type",
        "BrindleFox", "brindle fox", "Brindle-Fox", "BRINDLEFOX", "brindlefox's port",
        "SkarnVole", "skarn vole", "skarn_vole", "libSkarnVole", "SKARN-VOLE 2",
        "QuillMarrowFenDart", "quill-marrow-fen-dart", "Quill Marrow FenDart",
        # Glued affixes in lower case.
        "libbrindlefox", "mybrindlefox", "brindlefoxd", "skarnvoles", "the skarn voles",
        "libskarnvoles 1.0",
        # Every joiner.
        "gloam.tavern", "gloam+tavern", "gloam/tavern", "gloam%20tavern", "gloam‐tavern",
        "gloam‑tavern", "gloam tavern", "Quill.Marrow.Fen.Dart",
        "https://example.invalid/quill%20marrow%20fen%20dart",
        # A title name: Title Case when spaced, any case otherwise.
        "Cinder Ledger", "Cinder%20Ledger", "CinderLedger", "cinderledger", "cinder-ledger",
        "cinder_ledger", "cinder.ledger", "libcinderledger", "CINDER LEDGER",
    ]
    near_misses = (
        "a gloamy tavern\ntavern gloam\nbrindle, fox\nbrindle and fox\nskarn. Vole\n"
        "Quill Marrow Fen\ngloam\ntavern\nthe cinder ledger\nCinder ledger\n"
        "cinder%20ledger\ncinder ledger\nlibrary tavern\n"
    )
    samples = {
        "variants.md": "\n".join(variants) + "\n",
        "near-misses.md": near_misses,
        "docs/brindle_fox/notes.md": "Notes.\n",
        "src/GloamTavernClient.swift": "// Empty.\n",
        "art/brindlefox.png": b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x01",
        "pkg/Quill.Marrow/fen-dart.md": "Fine.\n",
        "docs/gloaming/tavern.md": "Fine.\n",
        "NOTICES.md": "Uses gloam-tavern 2.1 (MIT)\nBuilt like Gloam Tavern.\n",
        "vendor/skarn-vole/LICENSE": "Copyright the skarn-vole authors\n",
    }
    expected = {f"variants.md:{n}: forbidden name" for n in range(1, len(variants) + 1)} | {
        "docs/***/notes.md: forbidden name in the path",
        "src/***Client.swift: forbidden name in the path",
        "art/***.png: forbidden name in the path",
        "pkg/***.md: forbidden name in the path",
        "NOTICES.md:2: forbidden name",
    }
    words = [normalize(split_entry(entry)[0]) for entry in listed]
    failures = []

    def leaks_a_name(text):
        flat = normalize(text)
        return any(w in flat for w in words)

    def commit(root, message, *paths):
        if paths:
            git(root, "add", "--", *paths)
        git(
            root, "-c", "user.name=Self Test", "-c", "user.email=self-test@example.invalid",
            "-c", "commit.gpgsign=false", "commit", "-q", "--allow-empty", "-m", message,
        )
        return git(root, "rev-parse", "HEAD").strip()

    with tempfile.TemporaryDirectory() as scratch:
        root = os.path.join(scratch, "repo")
        os.mkdir(root)
        git(root, "init", "-q")
        plain = os.path.join(scratch, "plain-names")
        with open(plain, "w", encoding="utf-8") as handle:
            handle.write("# made-up names\n" + "\n".join(listed[:2]) + "  # trailing comment\n\n")
            handle.write("\n".join(listed[2:]) + "\n")
        hashed = os.path.join(scratch, "forbidden-names.txt")
        if write_names_file(plain, hashed, salt="5a17f0r7e57"):
            return len(listed), ["the made-up names were refused"]
        with open(hashed, "a", encoding="utf-8") as handle:
            handle.write("allow NOTICES.md \\(MIT\\)$\nallow vendor/*/LICENSE\nallow-commit ^Quoted:\n")
        # Rewriting keeps the salt, the flags and the allow lines.
        write_names_file(plain, hashed)
        with open(hashed, encoding="utf-8") as handle:
            stored = handle.read()
        if (
            "salt 5a17f0r7e57\n" not in stored or stored.count("\nallow ") != 2
            or stored.count("\nallow-commit ") != 1
            or len(re.findall(r"^[0-9a-f]{64} title$", stored, re.M)) != 1
        ):
            failures.append("--hash-names dropped the salt, a flag or an allow line")
        if leaks_a_name(stored):
            failures.append("the hash file spells a name")
        names = ForbiddenNames.load(hashed)
        if len(names.hashes) != len(listed) or len(names.title) != 1:
            failures.append(f"{len(names.hashes)} hashes, {len(names.title)} flagged, for {len(listed)} names")

        for path, content in samples.items():
            os.makedirs(os.path.dirname(os.path.join(root, path)) or root, exist_ok=True)
            with open(os.path.join(root, path), "wb") as handle:
                handle.write(content.encode() if isinstance(content, str) else content)
        first = commit(root, "Import the samples", *samples)
        files = git(root, "ls-files").split()

        hits = set(check(root, files, forbidden=names))
        failures += [f"not caught: {hit}" for hit in sorted(expected - hits)]
        failures += [f"false positive: {hit}" for hit in sorted(hits - expected)]
        failures += [f"a hit spells the name: {hit}" for hit in hits if leaks_a_name(hit)]
        if check(root, files):
            failures.append("forbidden names were checked without the rule")

        named = commit(root, "Port the Skarn Vole mixer\n\nDetails follow.")
        plain_msg = commit(root, "Tidy the docs\n\nA gloamy tavern, no names.")
        in_body = commit(root, "Tidy more\n\nFollows the gloam-tavern layout.")
        wrapped = commit(root, "Wrap a name\n\nThe layout follows the Gloam\nTavern one.")
        apart = commit(root, "Split a name\n\nEnds with gloam\n\nTavern starts here.")
        quoted = commit(root, "Quote a phrase\n\nQuoted: the gloam tavern sign\nAlso the skarn vole port.")
        lower = commit(root, "Add the cinder ledger view")
        title = commit(root, "Port the Cinder Ledger")
        zero = "0" * 40
        commit_cases = [
            (f"{first}..{in_body}", [f"commit {named[:12]}:1", f"commit {in_body[:12]}:3"]),
            (f"{named}..{plain_msg}", []),
            (f"{zero}..{plain_msg}", []),  # no origin/main: the head's own message
            (f"{zero}..{named}", [f"commit {named[:12]}:1"]),
            (f"{'1' * 40}..{in_body}", [f"commit {in_body[:12]}:3"]),
            (f"{in_body}..{wrapped}", [f"commit {wrapped[:12]}:3"]),  # wrapped onto line 4
            (f"{wrapped}..{apart}", []),  # a blank line between: two paragraphs
            (f"{apart}..{quoted}", [f"commit {quoted[:12]}:4"]),  # line 3 is waived
            (f"{quoted}..{lower}", []),  # a title name in lower case, spaced
            (f"{lower}..{title}", [f"commit {title[:12]}:1"]),
        ]
        for spec, want in commit_cases:
            got = sorted(hit.rsplit(": ", 1)[0] for hit in check_commits(root, spec, names))
            if got != sorted(want):
                failures.append(f"commits {spec[:10]}...: got {got}, want {want}")
        git(root, "update-ref", "refs/remotes/origin/main", first)
        got = sorted(hit.rsplit(": ", 1)[0] for hit in check_commits(root, f"{zero}..{in_body}", names))
        if got != sorted([f"commit {named[:12]}:1", f"commit {in_body[:12]}:3"]):
            failures.append(f"a new branch's first push wasn't checked from origin/main: {got}")

        for rule in ("allow docs/* ([a-z", "allow-commit (unclosed"):
            broken = os.path.join(scratch, "broken.txt")
            with open(broken, "w", encoding="utf-8") as handle:
                handle.write(f"# header\nsalt 5a17f0r7e57\n{rule}\n")
            try:
                ForbiddenNames.load(broken)
                failures.append(f"a bad pattern loaded: {rule.split()[0]}")
            except ValueError as error:
                if ":3: bad pattern" not in str(error):
                    failures.append(f"a bad pattern's report lacks its line: {rule.split()[0]}")
            except re.error:
                failures.append(f"a bad pattern raised re.error: {rule.split()[0]}")

        if verify_names(names, plain):
            failures.append("the plain list doesn't verify against its own hash file")
        unflagged = ForbiddenNames(names.salt, [h.hex() for h in names.hashes])
        if not any("flag differs" in p for p in verify_names(unflagged, plain)):
            failures.append("a flag missing from the hash file passed --verify-names")
        with open(plain, "a", encoding="utf-8") as handle:
            handle.write("Ochre Lantern\n")
        if len(verify_names(names, plain)) != 1:
            failures.append("a name missing from the hash file passed --verify-names")
        stale = ForbiddenNames(names.salt, [h.hex() for h in names.hashes] + ["ab" * 32])
        if not any("match no name" in p for p in verify_names(stale, plain)):
            failures.append("a stale hash passed --verify-names")
        bad = os.path.join(scratch, "bad-names")
        with open(bad, "w", encoding="utf-8") as handle:
            handle.write("Café Noir\nab\none two three four five\nPale Moth [loud]\n")
        problems = write_names_file(bad, os.path.join(scratch, "unused.txt"))
        if len(problems) != 4 or any(
            w in p.rsplit(": ", 1)[1] for p in problems for w in ("Caf", "ab", "five", "Moth", "loud")
        ):
            failures.append(f"bad plain-list entries: {len(problems)} problem(s)")
        if os.path.exists(os.path.join(scratch, "unused.txt")):
            failures.append("a refused plain list still wrote a hash file")

        missing = plain_list_problem(root, os.path.join(scratch, "no-such-list"))
        if not missing or "doesn't exist" not in missing:
            failures.append("a missing plain list wasn't reported as missing")
        inside = os.path.join(root, "names-here")
        with open(inside, "w", encoding="utf-8") as handle:
            handle.write("brindlefox\n")
        if plain_list_problem(root, inside) is None:
            failures.append("a plain list in the work tree, not ignored, was accepted")
        with open(os.path.join(root, ".gitignore"), "w", encoding="utf-8") as handle:
            handle.write("names-here\n")
        if plain_list_problem(root, inside) is not None:
            failures.append("an ignored plain list in the work tree was refused")
        if plain_list_problem(root, plain) is not None:
            failures.append("a plain list outside the work tree was refused")
        if plain_list_problem(root, os.path.join(root, "NOTICES.md")) is None:
            failures.append("a tracked file was accepted as the plain list")
    cases = len(variants) + near_misses.count("\n") + len(samples) + len(commit_cases) + 6
    return cases, failures


def main(argv):
    parser = argparse.ArgumentParser(
        description="Fail when a tracked file carries a private reference (see the docstring)."
    )
    parser.add_argument("files", nargs="*", metavar="FILE")
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--commits", metavar="RANGE")
    parser.add_argument("--hash-names", nargs="?", const="", metavar="PATH")
    parser.add_argument("--verify-names", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        return self_test()
    root = git(".", "rev-parse", "--show-toplevel").strip()
    names_file = os.path.join(root, NAMES_FILE)
    if args.hash_names is not None:
        plain = args.hash_names or local_plain_list(root)
        problems = (
            [plain_list_problem(root, plain)] if plain else
            ["no plain list: pass PATH, set $AUDIOSYNC_FORBIDDEN_NAMES or create "
             "info/forbidden-names in the git directory"]
        )
        if problems == [None]:
            problems = write_names_file(plain, names_file)
        for problem in problems:
            print(f"--hash-names: {problem}", file=sys.stderr)
        if problems:
            return 1
        print(f"{NAMES_FILE}: {len(ForbiddenNames.load(names_file).hashes)} hash(es) written")
        return 0
    try:
        forbidden = ForbiddenNames.load(names_file)
    except (OSError, ValueError) as error:
        print(f"forbidden names: {error}", file=sys.stderr)
        return 1
    if args.verify_names:
        plain = local_plain_list(root)
        if plain is None:
            print("forbidden names: no local plain list, verification skipped")
            return 0
        problems = [p for p in [plain_list_problem(root, plain)] if p] or verify_names(forbidden, plain)
        for problem in problems:
            print(f"forbidden names: {problem}", file=sys.stderr)
        if problems:
            return 1
        print(f"forbidden names: {NAMES_FILE} matches the local list ({len(forbidden.hashes)} names)")
        return 0
    if args.commits is not None:
        count = len(commit_messages(root, args.commits))
        hits = check_commits(root, args.commits, forbidden)
        for hit in hits:
            print(hit)
        if hits:
            print(f"{len(hits)} forbidden name(s) in commit messages; reword them", file=sys.stderr)
            return 1
        print(f"forbidden names: none in {count} commit message(s)")
        return 0
    argv = args.files
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
    hits = check(root, paths, names or (), hostname, explicit=bool(argv), forbidden=forbidden)
    for hit in hits:
        print(hit)
    if hits:
        print(f"{len(hits)} private reference(s); see the docstring of {SELF}", file=sys.stderr)
        return 1
    print(f"private references: none in {len(paths)} file(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
