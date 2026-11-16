#!/usr/bin/env python3
"""Fails when a source-text check in Tests/logictest.swift looks for text the source no longer has.

The logic suite reads Swift sources and checks `name.contains("literal")`. When the
code changes and the literal does not, the suite fails only on a Mac. This finds every
such check whose variable is read from a file, directly, through a helper function or
through a variable derived from one, and looks for each positive literal (one not under
`!`) in that file. A slice of a file is checked against the whole file, which can miss a
literal that moved out of the slice but never flags one that is there. Checks whose
variable does not resolve to a file are skipped.

Usage: python -I scripts/source_checks.py   (from the repository root)
"""

import os
import re
import sys

TEST = "Tests/logictest.swift"
SOURCES = "Sources/AudioSync/"

_PATH = re.compile(r'"((?:Sources/AudioSync/[\w+.-]+\.swift)|Info\.plist)"')
_DEF = re.compile(r"\b(?:let|var)\s+(?:\(([^)]*)\)|([A-Za-z_]\w*))")
_FUNC = re.compile(r"\bfunc\s+([A-Za-z_]\w*)\s*\(([^)]*)\)")
_CLOSURE = re.compile(r"\{\s*\(?([A-Za-z_][\w,\s]*)\)?\s+in\b")
_FOR = re.compile(r"\bfor\s+(?:\(([^)]*)\)|([A-Za-z_]\w*))\s+in\b")
_CHECK = re.compile(r'(!?)\s*\b([A-Za-z_]\w*)\.contains\("((?:[^"\\]|\\.)*)"\)')
_STRING = re.compile(r'"(?:[^"\\]|\\.)*"')
_IDENT = re.compile(r"[A-Za-z_]\w*")
_LEAD = re.compile(r"\s*\(*\s*([A-Za-z_]\w*)(?:\(\s*(?:[A-Za-z_]\w*\s*:\s*)?([A-Za-z_]\w*)\s*[,)])?")
_UNSUPPORTED = ("replacingOccurrences", "lowercased", "uppercased", "isWhitespace", "trimmingCharacters")


def unescape(literal):
    if "\\(" in literal:
        return None
    out, i = [], 0
    simple = {"n": "\n", "t": "\t", "r": "\r", '"': '"', "\\": "\\", "'": "'", "0": "\0"}
    while i < len(literal):
        c = literal[i]
        if c != "\\":
            out.append(c)
            i += 1
            continue
        nxt = literal[i + 1]
        if nxt in simple:
            out.append(simple[nxt])
            i += 2
        elif nxt == "u":
            end = literal.index("}", i)
            out.append(chr(int(literal[i + 3:end], 16)))
            i = end + 1
        else:
            return None
    return "".join(out)


def statement(lines, start):
    """The definition starting on line `start`, through its continuation lines."""
    text, depth, i = "", 0, start
    while i < len(lines):
        line = lines[i]
        text += line + "\n"
        code = _STRING.sub('""', line).split("//", 1)[0]
        depth += sum(code.count(c) for c in "([{") - sum(code.count(c) for c in ")]}")
        i += 1
        if depth <= 0 and (i >= len(lines) or not lines[i].lstrip().startswith((".", "?", "+", "&&"))):
            break
    return text


def strip_comment_lines(text):
    return "\n".join(l for l in text.split("\n") if not l.strip().startswith("//"))


class Resolver:
    def __init__(self, lines, root):
        self.lines, self.root, self.defs, self.cache, self.memo = lines, root, {}, {}, {}
        for n, line in enumerate(lines):
            code = line.split("//", 1)[0]
            for m in _FUNC.finditer(code):
                self.defs.setdefault(m.group(1), []).append((n, "func"))
                for param in m.group(2).split(","):
                    names = param.split(":", 1)[0].split()
                    if names:
                        self.defs.setdefault(names[-1], []).append((n, "param"))
            for pattern in (_DEF, _FOR, _CLOSURE):
                for m in pattern.finditer(code):
                    group = m.group(1) or (m.group(2) if pattern is not _CLOSURE else "")
                    closure = pattern is _DEF and code[m.end():].lstrip().startswith("= {")
                    for name in _IDENT.findall(group or ""):
                        self.defs.setdefault(name, []).append((n, "func" if closure else "value"))

    def nearest(self, name, line):
        found = [d for d in self.defs.get(name, []) if d[0] < line or (d[1] == "func" and d[0] <= line)]
        return max(found) if found else None

    def text(self, path, strip):
        key = (path, strip)
        if key not in self.cache:
            try:
                with open(os.path.join(self.root, path), encoding="utf-8") as f:
                    text = f.read()
            except OSError:
                text = None
            self.cache[key] = strip_comment_lines(text) if strip and text is not None else text
        return self.cache[key]

    def helper(self, name, line):
        """(fixed file, takes file name, takes path, strips comments) for a function reading a source."""
        d = self.nearest(name, line)
        if not d or d[1] != "func":
            return None
        body = statement(self.lines, d[0])
        if "contentsOfFile" not in body or "-> String" not in body.split("\n", 1)[0]:
            return None
        strip = 'hasPrefix("//")' in body
        fixed = _PATH.search(body)
        if fixed:
            return (fixed.group(1), False, False, strip)
        return (None, SOURCES + "\\(" in body, True, strip)

    def files(self, name, line, seen=()):
        """[(path, strip)] of the source text `name` holds at `line`, or None."""
        d = self.nearest(name, line)
        if not d or d[1] != "value" or (name, d[0]) in seen:
            return None
        if (name, d[0]) not in self.memo:
            self.memo[(name, d[0])] = self.resolve(name, d, seen)
        return self.memo[(name, d[0])]

    def resolve(self, name, d, seen):
        expr = statement(self.lines, d[0])
        expr = expr.split("=", 1)[1] if "=" in expr else ""
        if any(word in expr for word in _UNSUPPORTED) or "contentsOfDirectory" in expr:
            return None
        if "contentsOfFile" in expr:
            paths = _PATH.findall(expr)
            return [(p, False) for p in paths] if paths else None
        found = []
        for m in re.finditer(r'\b([A-Za-z_]\w*)\(\s*"([^"\\]+)"', expr):
            h = self.helper(m.group(1), d[0])
            if h:
                path = h[0] or (SOURCES + m.group(2) if h[1] else m.group(2))
                found.append((path, h[3]))
        if found:
            return found
        # A derived text is its receiver (`base.components(...)`) or a slicing call's first argument.
        lead = _LEAD.match(expr)
        if not lead:
            return None
        base = lead.group(2) or lead.group(1)
        return self.files(base, d[0], seen + ((name, d[0]),))


def main():
    root = os.getcwd()
    with open(os.path.join(root, TEST), encoding="utf-8") as f:
        lines = f.read().split("\n")
    resolver = Resolver(lines, root)
    verified, skipped, misses = 0, 0, []
    for n, line in enumerate(lines):
        code = line if not line.lstrip().startswith("//") else ""
        for m in _CHECK.finditer(code):
            if m.group(1):
                continue
            literal = unescape(m.group(3))
            files = resolver.files(m.group(2), n) if literal is not None else None
            texts = [resolver.text(p, s) for p, s in files] if files else []
            if not texts or None in texts:
                skipped += 1
                continue
            verified += 1
            if not any(literal in t for t in texts):
                misses.append((n + 1, m.group(2), m.group(3), ", ".join(sorted({p for p, _ in files}))))
    for number, name, literal, paths in misses:
        print(f'{TEST}:{number}: {name}.contains("{literal}") is not in {paths}')
    print(f"source checks: {verified} verified, {len(misses)} stale, {skipped} skipped (not read from a file)")
    return 1 if misses else 0


if __name__ == "__main__":
    sys.exit(main())
