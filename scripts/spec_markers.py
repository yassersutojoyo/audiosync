#!/usr/bin/env python3
"""Fails when a host `// covers:` marker names a clause its spec doesn't have.

The core, cast and airplay crates check their own markers in their
spec_coverage tests. This covers the rest: tracked Rust and Swift files
outside those crates (desktop/, remote/, Tests/, Sources/), and shell scripts,
whose marker is `# covers:`. The letter picks
the spec: S is docs/ENGINE_SPEC.md, P is docs/AIRPLAY_SPEC.md, C is
docs/CAST_SPEC.md. A withdrawn, retired or superseded clause counts as
missing (one retired or superseded in part does not). Each ID may carry a note in parentheses, and a
note may run on to the next lines:

    // covers: S290, S291 (a level set on the speaker is kept)

Usage:
    scripts/spec_markers.py              check every tracked file in scope
    scripts/spec_markers.py --self-test  check the parser on made-up input
"""

import argparse
import os
import re
import subprocess
import sys

MARKER = "// covers:"
SHELL_MARKER = "# covers:"
SPECS = {"S": "ENGINE_SPEC.md", "P": "AIRPLAY_SPEC.md", "C": "CAST_SPEC.md"}
CRATES_WITH_OWN_CHECK = ("core/", "cast/", "airplay/")
_ID = re.compile(r"[SPC][1-9][0-9]*[a-z]?")
_CLAUSE = re.compile(r"\s*- \*\*([SPC][1-9][0-9]*[a-z]?)\*\*(.*)")
# Only the opening words: a live clause may mention withdrawn things later on.
_WITHDRAWN = re.compile(r"\s*[*(]*(withdrawn|(retired|superseded)(?! in part))", re.IGNORECASE)


def in_scope(path):
    return path.endswith((".rs", ".swift", ".sh")) and not path.startswith(CRATES_WITH_OWN_CHECK)


def marker_for(path):
    return SHELL_MARKER if path.endswith(".sh") else MARKER


def marker_ids(body):
    """The IDs of one marker line's text after `covers:`, or None if malformed."""
    depth = 0
    kept = []
    for char in body:
        if char == "(":
            depth += 1
        elif char == ")":
            if depth == 0:
                return None
            depth -= 1
        elif depth == 0:
            kept.append(char)
    items = [item.strip() for item in "".join(kept).split(",")]
    if not all(_ID.fullmatch(item) for item in items):
        return None
    return items


def spec_ids(text):
    """Clause IDs defined in a spec, withdrawn ones left out."""
    found = set()
    for line in text.splitlines():
        match = _CLAUSE.match(line)
        if match and not _WITHDRAWN.match(match.group(2)):
            found.add(match.group(1))
    return found


def problems_in(path, text, known):
    marker = marker_for(path)
    found = []
    for number, line in enumerate(text.splitlines(), 1):
        stripped = line.strip()
        if not stripped.startswith(marker):
            continue
        ids = marker_ids(stripped[len(marker):])
        if ids is None:
            found.append(f"{path}:{number}: malformed marker: {stripped}")
            continue
        found += [
            f"{path}:{number}: {clause} is not a clause of {SPECS[clause[0]]}"
            for clause in ids
            if clause not in known
        ]
    return found


def tracked_files(root):
    out = subprocess.run(
        ["git", "-C", root, "ls-files", "-z"], capture_output=True, check=True
    ).stdout
    return [p for p in out.decode("utf-8").split("\0") if p]


def self_test():
    failures = []
    ids = {
        " S290, S291 (a level, kept (twice))": ["S290", "S291"],
        " S246 (says why), S247 (a note that runs on": ["S246", "S247"],
        " C77a, P213": ["C77a", "P213"],
        " S12)": None,
        " S12 S13": None,
        " S012": None,
        " X1": None,
        "": None,
        " S1,": None,
    }
    for body, want in ids.items():
        got = marker_ids(body)
        if got != want:
            failures.append(f"marker_ids({body!r}): expected {want}, got {got}")
    spec = (
        "- **S1** One.\n  - **S2** Two.\n- **S3** *Withdrawn*: gone.\n- **G1** Gap.\n"
        "- **S4** *Required.* Replaces the withdrawn S3.\n"
        "- **S5** *Retired with the bundled engine; kept as a record.*\n"
        "- **S6** *Retired in part with the bundled engine*: the rest holds.\n"
        "- **S7** *Superseded by S4.* *Observed* (the old rule).\n"
        "- **S8** *Superseded in part by S4*: the rest holds.\n"
    )
    if spec_ids(spec) != {"S1", "S2", "S4", "S6", "S8"}:
        failures.append(f"spec_ids: got {sorted(spec_ids(spec))}")
    text = "// covers: S1\n    // covers: S2, S3\n/// What it covers: x\n//covers:S1\n"
    got = problems_in("a.rs", text, {"S1", "S2"})
    want = ["a.rs:2: S3 is not a clause of ENGINE_SPEC.md"]
    if got != want:
        failures.append(f"problems_in: expected {want}, got {got}")
    got = problems_in("t.sh", "# covers: S1, S3\n// covers: S3\n", {"S1"})
    want = ["t.sh:1: S3 is not a clause of ENGINE_SPEC.md"]
    if got != want:
        failures.append(f"problems_in (shell): expected {want}, got {got}")
    scope = {
        "desktop/src/view.rs": True,
        "Tests/logictest.swift": True,
        "remote/tests/server.rs": True,
        "core/tests/spec_coverage.rs": False,
        "cast/src/lib.rs": False,
        "airplay/src/transport.rs": False,
        "desktop/README.md": False,
        "Tests/shellchecktest.sh": True,
        "core/ffi/build.sh": False,
    }
    for path, want in scope.items():
        if in_scope(path) != want:
            failures.append(f"scope of {path}: expected {want}")
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    cases = len(ids) + 3 + len(scope)
    print(f"self-test: {cases} cases, {len(failures)} failure(s)")
    return 1 if failures else 0


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        return self_test()
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    known = set()
    for spec in SPECS.values():
        with open(os.path.join(root, "docs", spec), encoding="utf-8") as f:
            known |= spec_ids(f.read())
    found = []
    markers = 0
    for path in tracked_files(root):
        if not in_scope(path):
            continue
        with open(os.path.join(root, path), encoding="utf-8") as f:
            text = f.read()
        marker = marker_for(path)
        markers += sum(1 for line in text.splitlines() if line.strip().startswith(marker))
        found += problems_in(path, text, known)
    for problem in found:
        print(problem)
    print(f"{markers} host markers, {len(found)} problem(s)")
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
