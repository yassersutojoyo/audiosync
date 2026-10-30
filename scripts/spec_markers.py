#!/usr/bin/env python3
"""Fails when a host `// covers:` marker names a clause its spec doesn't have.

The core, cast and airplay crates check the markers in their own src/ and
tests/ in their spec_coverage tests. This covers the rest: tracked Rust, Swift
and C files elsewhere (crate examples and benches included), and shell and
Python files, whose marker is `# covers:`. The letter picks
the spec: S is docs/ENGINE_SPEC.md, P is docs/AIRPLAY_SPEC.md, C is
docs/CAST_SPEC.md. A withdrawn, retired or superseded clause counts as
missing (one retired or superseded in part does not). Each ID may carry a note in parentheses, and a
note may run on to the next lines:

    // covers: S290, S291 (a level set on the speaker is kept)

It also fails when a row of a spec's test map names a test function that no
tracked Rust file defines, or one defined outside the row's backticked path or
the host its tag or section names (names in parentheses are notes, and a
*hardware* row names a check run by hand); when a row of a Check table names a
check that no string in Tests/, or in a script run-tests.sh calls, prints (`…`,
`<…>` and interpolations match anything); and when a section repeats a row.

Outside the test maps, a live clause that cites a test right after "test",
"check" or "suite" (or in a list after one) must name a real one: a check name
in quotes, or a Rust test in words ("render start: a late output …" for a fn
starting a_late_output in render_start.rs), or a fn name in backticks.

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
HASH_MARKER = "# covers:"
SPECS = {"S": "ENGINE_SPEC.md", "P": "AIRPLAY_SPEC.md", "C": "CAST_SPEC.md"}
CRATES_WITH_OWN_CHECK = tuple(f"{c}/{d}/" for c in ("core", "cast", "airplay") for d in ("src", "tests"))
_ID = re.compile(r"[SPC][1-9][0-9]*[a-z]?")
_CLAUSE = re.compile(r"\s*- \*\*([SPC][1-9][0-9]*[a-z]?)\*\*(.*)")
# Only the opening words: a live clause may mention withdrawn things later on.
_WITHDRAWN = re.compile(r"\s*[*(]*(withdrawn|(retired|superseded)(?! in part))", re.IGNORECASE)
_TEST_MAP = re.compile(r"## [0-9]+\. Test map")
_FN = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)")
# Snake case with at least one underscore, not part of a path or a longer word.
_TEST_NAME = re.compile(r"(?<![\w./-])[a-z][a-z0-9]*(?:_[a-z0-9]+)+(?![\w./-])")
_ONE_WORD = re.compile(r"[a-z][a-z0-9]*")
_PATH = re.compile(r"`([\w./-]+(?:\.rs|/))`")
# A clause cites a test by its check name in quotes or its fn name in backticks, right
# after "test", "check" or "suite" or in a list that follows one.
_CITED = re.compile(r'"([^"]+)"|`([a-z][a-z0-9]*(?:_[a-z0-9]+)+)`')
_CITES = re.compile(r"\b(tests?|checks?|suite)[,:]? \(?$", re.IGNORECASE)
_LISTED = re.compile(r",? (and |or )?")


def in_scope(path):
    return path.endswith((".rs", ".swift", ".c", ".h", ".sh", ".py")) and not path.startswith(CRATES_WITH_OWN_CHECK)


def marker_for(path):
    return HASH_MARKER if path.endswith((".sh", ".py")) else MARKER


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
        if match and not _withdrawn(match.group(2)):
            found.add(match.group(1))
    return found


def _withdrawn(after_id):
    """Same rule as core/tests/spec_coverage.rs: the opening may follow a title."""
    body = after_id.lstrip()
    return bool(_WITHDRAWN.match(body) or _WITHDRAWN.match(_after_title(body)))


def _after_title(text):
    """The text after a leading italic title that ends with a period, or all of it."""
    if text.startswith("*"):
        title, closed, after = text[1:].partition("*")
        if closed and len(title) > 1 and title.endswith("."):
            return after.lstrip()
    return text


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


def _outside_parens(text):
    depth = 0
    kept = []
    for char in text:
        if char == "(":
            depth += 1
        elif char == ")":
            depth = max(depth - 1, 0)
        elif depth == 0:
            kept.append(char)
    return "".join(kept)


def _pieces(text):
    """The cell as (text outside parentheses, the parenthesized group after it) pairs."""
    pieces = []
    depth = 0
    outside, group = [], []
    for char in text:
        if char == "(":
            depth += 1
            if depth == 1:
                continue
        elif char == ")" and depth:
            depth -= 1
            if depth == 0:
                pieces.append(("".join(outside), "".join(group)))
                outside, group = [], []
                continue
        (group if depth else outside).append(char)
    pieces.append(("".join(outside), "".join(group)))
    return pieces


def _host_dirs(words):
    dirs = ("desktop/",) if re.search(r"\bWindows\b", words) else ()
    dirs += ("core/",) if re.search(r"\bcore\b", words) else ()
    return dirs + tuple(f"{name.lower()}/" for name in re.findall(r"\b(remote|AirPlay|Cast) crate\b", words))


def _defined_in(paths, scope):
    """Whether one of `paths` lies in `scope`: backticked file paths or host directories."""
    return any(p == s or p.endswith("/" + s) or (s.endswith("/") and p.startswith(s)) for p in paths for s in scope)


def test_map_problems(spec, text, fns, labels=None):
    """Test-map rows of one spec that name a function missing from `fns` or outside the
    row's host, a check label missing from `labels`, or repeat an earlier row."""
    found = []
    in_map = False
    scope = ()
    checks = False
    seen = {}
    for number, line in enumerate(text.splitlines(), 1):
        if line.startswith("## "):
            in_map = bool(_TEST_MAP.match(line))
        if line.startswith(("## ", "### ")):
            # The host comes after the topic: "Start steps and late Windows wakes: core (...)".
            scope = _host_dirs(line.split("(")[0].rpartition(": ")[2]) if line.startswith("### ") else ()
            checks = False
            seen = {}
            continue
        cells = line.split("|")
        if not in_map or len(cells) < 3 or cells[0].strip():
            continue
        label = cells[1].strip()
        if label in ("Check", "Test", "Tests"):
            checks = label == "Check"
            continue
        if set(label) <= set("-: "):
            continue
        if len(cells) > 3 and cells[3].strip().startswith("*hardware*"):
            continue
        if label in seen:
            found.append(f"docs/{spec}:{number}: the test map repeats the row of line {seen[label]}")
        seen.setdefault(label, number)
        names = _outside_parens(cells[1])
        # A one-word name has no underscore for _TEST_NAME, so it counts only in a cell
        # that is nothing but a list of names; prose cells say "once" or "max" too.
        items = [item.strip() for item in names.split(",")]
        listed = all(_ONE_WORD.fullmatch(item) or _TEST_NAME.fullmatch(item) for item in items)
        named = []
        for outside, group in _pieces(cells[1]):
            pending = _TEST_NAME.findall(outside)
            pending += [w for w in (i.strip() for i in outside.split(",")) if listed and _ONE_WORD.fullmatch(w)]
            paths = tuple(_PATH.findall(group))
            tag = _host_dirs(group) if group.strip() in ("core", "Windows") else ()
            named += [(name, paths or tag) for name in pending]
        for name, where in named:
            if name not in fns:
                found.append(f"docs/{spec}:{number}: the test map names {name}, which no tracked Rust file defines")
            elif (where or scope) and not _defined_in(fns[name], where or scope):
                found.append(
                    f"docs/{spec}:{number}: the test map puts {name} in {', '.join(where or scope)},"
                    f" but it is defined in {', '.join(sorted(fns[name]))}"
                )
        # A row that starts with a command names a script run, and a pending check prints nothing yet.
        if checks and not named and labels is not None and not label.startswith("`") and "(pending" not in label:
            if not any(_label_found(form, labels) for form in _label_forms(label)):
                found.append(f'docs/{spec}:{number}: the test map names the check "{label}", which no test prints')
    return found


def citation_problems(spec, text, fns, labels):
    """Test names a live clause cites in its own text that no test defines or prints."""
    lines = text.splitlines()
    tables = {line[4:].split(" (")[0] for line in lines if line.startswith("### ")}
    found = []
    in_map = live = False
    for number, line in enumerate(lines, 1):
        if line.startswith("## "):
            in_map = bool(_TEST_MAP.match(line))
        clause = _CLAUSE.match(line)
        if clause:
            live = not _withdrawn(clause.group(2))
        elif not line.startswith(" "):
            live = False
        if in_map or not live:
            continue
        listed = -1
        for match in _CITED.finditer(line):
            before = line[: match.start()]
            if not (_CITES.search(before) or _LISTED.fullmatch(before[listed:]) and listed >= 0):
                continue
            listed = match.end()
            name = match.group(1) or match.group(2)
            if name not in fns and name not in tables and not _cited_test_found(name, fns, labels):
                found.append(f'docs/{spec}:{number}: a clause cites the test "{name}", which no test defines or prints')
    return found


def _cited_test_found(name, fns, labels):
    """A check label, or a Rust test fn in words: "render start: a late output …" is a fn
    starting a_late_output in render_start.rs, and "render start: …" the file itself.
    An ellipsis needs a module before it: "F …" alone would match nearly anything."""
    name = name.replace("...", "…")
    if "…" in name and not re.match(r"[^…:]*\w[^…:]*: ", name):
        return False
    for form in _label_forms(name):
        if _label_found(form.replace(" …", "…"), labels):
            return True
        module, _, words = form.rpartition(": ")
        stem = _snake(module)
        parts = [_snake(part) for part in words.split("…")]
        code = re.compile(".*".join(parts))
        for fn, paths in fns.items():
            in_file = any(os.path.splitext(os.path.basename(p))[0] == stem for p in paths)
            if (in_file or not stem) and (code.fullmatch(fn) or words.strip() == "…"):
                return True
    return False


def _snake(words):
    """Words as a pattern of a snake-case name; an apostrophe may become _ or vanish."""
    return re.sub(r"[^a-z0-9']+", "_", words.lower()).strip("_'").replace("'", "_?")


def _label_forms(label):
    """The label, then without its trailing notes: parenthesized ones and ", every check"."""
    label = re.sub(r"\\([<>])", r"\1", label).removesuffix(", every check")
    forms = [label]
    while note := re.fullmatch(r"(.*\S)\s*\([^()]*\)", forms[-1]):
        forms.append(note.group(1))
    return forms + [" ".join(_outside_parens(label).split())]


def _label_found(label, labels):
    exact, patterns = labels
    if label in exact:
        return True
    wild = "…" in label or "<" in label
    if wild:
        doc = re.compile(".*".join(re.escape(part) for part in re.split(r"…|<[^>]*>", label)), re.DOTALL)
        if any(doc.fullmatch(text) for text in exact):
            return True
        label = re.sub(r"…|<[^>]*>", "\0", label)
    # A printed string that is mostly interpolation would match any label, so it only
    # meets a label with wildcards of its own: "\(name): delay \(ms) ms", "<device>: delay 150 ms".
    usable = [code for code, letters, leading in patterns if letters >= 8 and not leading or wild and letters >= 4]
    return any(code.fullmatch(label) for code in usable)


def string_literals(text, shell):
    """String literals of a Swift or shell file, interpolations as \\0, literals joined by + as one."""
    found = []
    i, n = 0, len(text)
    while i < n:
        char = text[i]
        comment = text[i - 1 : i] in ("", " ", "\t", "\n") and char == "#" if shell else text.startswith("//", i)
        if comment:
            end = text.find("\n", i)
            i = n if end < 0 else end
        elif not shell and (text.startswith('"""', i) or text.startswith('#"', i)):
            close = '"""' if text.startswith('"""', i) else ('"""#' if text.startswith('#"""', i) else '"#')
            end = text.find(close, i + len(close))
            i = n if end < 0 else end + len(close)
        elif shell and char == "'":
            end = text.find("'", i + 1)
            end = n if end < 0 else end
            found.append((i, end + 1, text[i + 1:end]))
            i = end + 1
        elif char == '"':
            start = i
            value, i = _read_literal(text, i + 1, shell)
            found.append((start, i, value))
        else:
            i += 1
    joined = []
    for start, end, value in found:
        if joined and re.fullmatch(r"\s*\+\s*", text[joined[-1][1]:start]):
            joined[-1] = (joined[-1][0], end, joined[-1][2] + value)
        else:
            joined.append((start, end, value))
    return [value for _, _, value in joined]


def _read_literal(text, i, shell):
    """The value of a double-quoted literal from after its quote, and the index after its end."""
    out = []
    n = len(text)
    while i < n and text[i] not in '"\n':
        char = text[i]
        opens = (not shell and text.startswith("\\(", i)) or (shell and text.startswith(("$(", "${"), i))
        if opens:
            depth, i = 1, i + 2
            while i < n and depth and text[i] != "\n":
                if text[i] == '"':
                    _, i = _read_literal(text, i + 1, shell)
                    continue
                depth += {"(": 1, "{": 1, ")": -1, "}": -1}.get(text[i], 0)
                i += 1
            out.append("\0")
        elif shell and char == "$" and i + 1 < n and re.match(r"[\w@#?*!-]", text[i + 1]):
            i += 1 + len(re.match(r"\w*|.", text[i + 1:]).group())
            out.append("\0")
        elif char == "\\" and i + 1 < n:
            out.append({"n": "\n", "t": "\t"}.get(text[i + 1], text[i + 1]))
            i += 2
        else:
            out.append(char)
            i += 1
    return "".join(out), i + 1


def check_labels(texts):
    """Every printed check name of the Swift and shell tests: exact texts and patterns."""
    exact = set()
    patterns = []
    for path, text in texts:
        for value in string_literals(text, path.endswith(".sh")):
            value = value.removeprefix("▶ ").strip()
            exact.add(value)
            if "\0" in value:
                code = re.compile(".*".join(re.escape(p) for p in value.split("\0")), re.DOTALL)
                patterns.append((code, sum(c.isalpha() for c in value), value.startswith("\0")))
    return exact, patterns


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
    titled = {
        "- **S9** *Engine restart.* *Retired with the bundled engine.*": set(),
        "- **S9** *Network loss, as tested.* *Retired in part*: the hint stands.": {"S9"},
    }
    for line, want in titled.items():
        if spec_ids(line) != want:
            failures.append(f"spec_ids({line!r}): expected {want}, got {spec_ids(line)}")
    text = "// covers: S1\n    // covers: S2, S3\n/// What it covers: x\n//covers:S1\n"
    got = problems_in("a.rs", text, {"S1", "S2"})
    want = ["a.rs:2: S3 is not a clause of ENGINE_SPEC.md"]
    if got != want:
        failures.append(f"problems_in: expected {want}, got {got}")
    got = problems_in("t.sh", "# covers: S1, S3\n// covers: S3\n", {"S1"})
    want = ["t.sh:1: S3 is not a clause of ENGINE_SPEC.md"]
    if got != want:
        failures.append(f"problems_in (shell): expected {want}, got {got}")
    got = problems_in("b.py", "    # covers: S1\n# covers: S9\n// covers: S9\n", {"S1"})
    want = ["b.py:2: S9 is not a clause of ENGINE_SPEC.md"]
    if got != want:
        failures.append(f"problems_in (Python): expected {want}, got {got}")
    scope = {
        "desktop/src/view.rs": True,
        "Tests/logictest.swift": True,
        "remote/tests/server.rs": True,
        "core/tests/spec_coverage.rs": False,
        "cast/src/lib.rs": False,
        "airplay/src/transport.rs": False,
        "desktop/README.md": False,
        "Tests/shellchecktest.sh": True,
        "core/ffi/smoke.sh": True,
        "core/ffi/swift-smoke/main.swift": True,
        "airplay/examples/fake_tv/cells.rs": True,
        "core/benches/eq.rs": True,
        "Tests/panic-probe.c": True,
        "core/tests/support/mod.rs": False,
        "scripts/check_appcast.py": True,
    }
    for path, want in scope.items():
        if in_scope(path) != want:
            failures.append(f"scope of {path}: expected {want}")
    doc = (
        "## 9. Notes\n| a_gone_name | S1 |\n## 10. Test map\n| Test | Clauses |\n|---|---|\n"
        "| a_real_test, a_gone_test (`x/y_z.rs`) | S1 |\n| a check (by a_note_name) | S2 |\n"
        "| a_hand_check | P1 | *hardware*: by ear |\n| gone | S3 |\n| a_real_test, then once more | S4 |\n"
        "## 11. After\n| another_gone | S1 |\n"
    )
    got = test_map_problems("X_SPEC.md", doc, {"a_real_test": {"x/y_z.rs"}})
    want = [
        "docs/X_SPEC.md:6: the test map names a_gone_test, which no tracked Rust file defines",
        "docs/X_SPEC.md:9: the test map names gone, which no tracked Rust file defines",
    ]
    if got != want:
        failures.append(f"test_map_problems: expected {want}, got {got}")
    swift = (
        'check(ok, "a: one")\ncheck(ok, "b: two \\(f("x")) and "\n    + "more") // "c: not a label"\n'
        'check(ok, "\\(name): delay \\(ms) ms")\n'
    )
    shell = "echo \"▶ e: the $thing runs\"  # 'not one'\necho 'f: as is'\n"
    labels = check_labels([("Tests/t.swift", swift), ("Tests/t.sh", shell)])
    if labels[0] != {"a: one", "b: two \0 and more", "\0: delay \0 ms", "e: the \0 runs", "f: as is"}:
        failures.append(f"check_labels: got {sorted(labels[0])}")
    fns = {"core_fn": {"core/src/a.rs"}, "win_fn": {"desktop/src/b.rs"}}
    doc = (
        "## 18. Test map\n### Things: Windows (`cargo test` in `desktop/`)\n| Test | Clauses |\n|---|---|\n"
        "| win_fn | S1 |\n| core_fn | S1 |\n| core_fn (core) | S1 |\n| win_fn (`core/src/a.rs`) | S1 |\n"
        "| Check | Clauses |\n|---|---|\n| a: one (a note) | S1 |\n| \\<device\\>: delay 150 ms | S1 |\n"
        "| b: two … more, every check | S1 |\n| a: gone | S1 |\n| a: one (a note) | S2 |\n"
        "| `x.py --self-test` | S1 |\n| g: later (pending, [MAC]) | S1 |\n"
    )
    got = test_map_problems("X_SPEC.md", doc, fns, labels)
    want = [
        "docs/X_SPEC.md:6: the test map puts core_fn in desktop/, but it is defined in core/src/a.rs",
        "docs/X_SPEC.md:8: the test map puts win_fn in core/src/a.rs, but it is defined in desktop/src/b.rs",
        'docs/X_SPEC.md:14: the test map names the check "a: gone", which no test prints',
        "docs/X_SPEC.md:15: the test map repeats the row of line 11",
    ]
    if got != want:
        failures.append(f"test_map_problems (hosts and checks): expected {want}, got {got}")
    swift = 'check(ok, "\\(a), \\(b), \\(c), \\(d), \\(e)")\ncheck(ok, "airplay \\(x) \\(y)")\n'
    labels = check_labels([("Tests/t.swift", swift), ("Tests/t.sh", 'echo "$x is missing"\n')])
    doc = (
        "## 18. Test map\n| Check | Clauses |\n|---|---|\n| gone: a, b, c, d, e | S1 |\n"
        "| airplay gone stale row | S1 |\n| eq: a renamed thing is missing | S1 |\n"
    )
    got = [line.split(":")[1] for line in test_map_problems("X_SPEC.md", doc, {}, labels)]
    if got != ["4", "5", "6"]:
        failures.append(f"test_map_problems (catch-all strings): expected lines 4 to 6, got {got}")
    fns = {"a_late_output_never_runs_dry": {"core/tests/render_start.rs"}, "an_output_s_lead": {"x.rs"}}
    doc = (
        '- **S1** One (core tests "render start: a late output …", `an_output_s_lead`, "a: one").\n'
        '- **S2** Two (tests "render: a late output …" and "a gone check"; a "quoted word").\n'
        '- **S3** *Withdrawn* (test "a gone check").\n'
        '- **S4** Four (Tests: "x gone"; test "a …"; check "…"; tests (`gone_fn`); suite "a: …").\n'
        '## 18. Test map\n| a_gone_name | S1 |\n'
    )
    got = citation_problems("X_SPEC.md", doc, fns, check_labels([("Tests/t.swift", 'check(ok, "a: one")\n')]))
    want = [
        'docs/X_SPEC.md:2: a clause cites the test "render: a late output …", which no test defines or prints',
        'docs/X_SPEC.md:2: a clause cites the test "a gone check", which no test defines or prints',
    ] + [
        f'docs/X_SPEC.md:4: a clause cites the test "{name}", which no test defines or prints'
        for name in ("x gone", "a …", "…", "gone_fn")
    ]
    if got != want:
        failures.append(f"citation_problems: expected {want}, got {got}")
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    cases = len(ids) + 9 + len(titled) + len(scope)
    print(f"self-test: {cases} cases, {len(failures)} failure(s)")
    return 1 if failures else 0


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        return self_test()
    # Labels may hold characters a Windows console code page can't print.
    sys.stdout.reconfigure(errors="backslashreplace")
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    known = set()
    for spec in SPECS.values():
        with open(os.path.join(root, "docs", spec), encoding="utf-8") as f:
            known |= spec_ids(f.read())
    found = []
    markers = 0
    fns = {}
    tests = {}
    for path in tracked_files(root):
        if not (in_scope(path) or path.endswith(".rs")):
            continue
        with open(os.path.join(root, path), encoding="utf-8") as f:
            text = f.read()
        if path.endswith(".rs"):
            for name in _FN.findall(text):
                fns.setdefault(name, set()).add(path)
        if re.fullmatch(r"(Tests/[^/]+\.swift|(Tests|scripts)/[^/]+\.sh)", path):
            tests[path] = text
        if not in_scope(path):
            continue
        marker = marker_for(path)
        markers += sum(1 for line in text.splitlines() if line.strip().startswith(marker))
        found += problems_in(path, text, known)
    # A check run-tests.sh prints may come from a script it calls, such as the shellcheck run.
    called = set(re.findall(r"scripts/[\w-]+\.sh", tests.get("Tests/run-tests.sh", "")))
    labels = check_labels([(p, t) for p, t in tests.items() if p.startswith("Tests/") or p in called])
    for spec in SPECS.values():
        with open(os.path.join(root, "docs", spec), encoding="utf-8") as f:
            text = f.read()
        found += test_map_problems(spec, text, fns, labels)
        found += citation_problems(spec, text, fns, labels)
    for problem in found:
        print(problem)
    print(f"{markers} host markers, {len(found)} problem(s)")
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
