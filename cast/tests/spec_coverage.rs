//! Spec coverage: every clause of `docs/CAST_SPEC.md` (C1 to Cn) is named
//! by at least one `// covers: C20, C26` line in `cast/src` or
//! `cast/tests`, or is listed in `cast/tests/spec_pending.txt` with a
//! one-line reason because a later stage implements it (the stream server,
//! media, volume, timing, recovery, the host), or in
//! `cast/tests/spec_exempt.txt` because only the host applies it and the
//! host's own tests cover it. A clause is covered
//! only when everything it asks of the sender is implemented and tested; a
//! test for part of a pending clause says so in a plain comment instead of a
//! marker. The pending list may only shrink: a clause that gains a marker
//! must leave it.
//!
//! A clause inserted between two others takes the earlier one's number and the
//! next unused lowercase letter (`C77a`, then `C77b`). Tests in `core/` name
//! Cast clauses on a `// CAST_SPEC: C77a` line: they are checked to name spec
//! clauses, but don't count as coverage, since this check reads only the cast
//! crate.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

const MARKER: &str = "// covers:";
const CORE_MARKER: &str = "// CAST_SPEC:";

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn spec_text() -> String {
    let path = crate_dir().join("..").join("docs").join("CAST_SPEC.md");
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// A clause ID: `C44` is number 44, `C77a` is number 77 with suffix `a`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Id {
    number: u32,
    suffix: Option<char>,
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "C{}", self.number)?;
        match self.suffix {
            Some(suffix) => write!(f, "{suffix}"),
            None => Ok(()),
        }
    }
}

fn clause_id(id: &str) -> Option<Id> {
    let rest = id.strip_prefix('C')?;
    let (digits, suffix) = match rest.char_indices().last()? {
        (at, last) if last.is_ascii_lowercase() => (&rest[..at], Some(last)),
        _ => (rest, None),
    };
    let first = digits.chars().next()?;
    if first == '0' || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(Id {
        number: digits.parse().ok()?,
        suffix,
    })
}

/// One clause: its ID and its text, from its `- **C12**` line up to the
/// next clause or heading (tables and continuation lines included).
struct Clause {
    id: Id,
    text: String,
}

fn clauses(spec: &str) -> Vec<Clause> {
    let mut found: Vec<Clause> = Vec::new();
    let mut open = false;
    for line in spec.lines() {
        let id = line
            .trim_start()
            .strip_prefix("- **")
            .and_then(|rest| rest.split_once("**"))
            .and_then(|(id, _)| clause_id(id));
        if let Some(id) = id {
            found.push(Clause {
                id,
                text: line.to_owned(),
            });
            open = true;
        } else if line.starts_with('#') || line.trim_start().starts_with("- **G") {
            open = false;
        } else if open {
            let clause = found.last_mut().expect("open clause");
            clause.text.push('\n');
            clause.text.push_str(line);
        }
    }
    found
}

fn is_withdrawn(clause: &Clause) -> bool {
    clause
        .text
        .lines()
        .next()
        .is_some_and(|line| line.to_ascii_lowercase().contains("withdrawn"))
}

fn rust_files(dir: &Path, found: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("{}: {error}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
}

/// Clauses named by `marker` lines in the Rust files under `dirs` (relative
/// to the cast crate), with where each was named; and every malformed marker
/// line.
fn marked(dirs: &[&str], marker: &str) -> (BTreeMap<Id, Vec<String>>, Vec<String>) {
    let mut files = Vec::new();
    for dir in dirs {
        rust_files(&crate_dir().join(dir), &mut files);
    }
    let mut covered: BTreeMap<Id, Vec<String>> = BTreeMap::new();
    let mut malformed = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).expect("readable source file");
        let name = file
            .strip_prefix(crate_dir())
            .unwrap_or(&file)
            .display()
            .to_string();
        for (index, line) in text.lines().enumerate() {
            let Some(list) = line.trim_start().strip_prefix(marker) else {
                continue;
            };
            let place = format!("{name}:{}", index + 1);
            let ids: Vec<Option<Id>> = match list.strip_prefix(' ') {
                Some(list) => list.split(", ").map(clause_id).collect(),
                None => vec![None],
            };
            if ids.iter().any(Option::is_none) {
                malformed.push(format!("{place}: {}", line.trim()));
                continue;
            }
            for id in ids.into_iter().flatten() {
                covered.entry(id).or_default().push(place.clone());
            }
        }
    }
    (covered, malformed)
}

fn covers() -> (BTreeMap<Id, Vec<String>>, Vec<String>) {
    marked(&["src", "tests"], MARKER)
}

/// The `// CAST_SPEC:` lines of the core, which this check validates but
/// doesn't count.
fn core_markers() -> (BTreeMap<Id, Vec<String>>, Vec<String>) {
    marked(&["../core/src", "../core/tests"], CORE_MARKER)
}

/// The pending list: clause ID to reason; and every malformed line.
fn pending() -> (BTreeMap<Id, String>, Vec<String>) {
    clause_list("spec_pending.txt")
}

/// The exempt list: clause ID to the host test that covers it; and every
/// malformed line.
fn exemptions() -> (BTreeMap<Id, String>, Vec<String>) {
    clause_list("spec_exempt.txt")
}

/// A clause list in `tests/` (`C<n> <reason>` per line, `#` comments):
/// clause ID to reason; and every malformed line.
fn clause_list(file: &str) -> (BTreeMap<Id, String>, Vec<String>) {
    let text = fs::read_to_string(crate_dir().join("tests").join(file))
        .unwrap_or_else(|error| panic!("tests/{file}: {error}"));
    let mut listed = BTreeMap::new();
    let mut malformed = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (id, reason) = line.split_once(' ').unwrap_or((line, ""));
        match clause_id(id) {
            Some(id_parsed) if !reason.trim().is_empty() => {
                if listed.insert(id_parsed, reason.trim().to_owned()).is_some() {
                    malformed.push(format!("line {}: {id} is listed twice", index + 1));
                }
            }
            _ => malformed.push(format!(
                "line {}: expected `C<n> <reason>`, got `{line}`",
                index + 1
            )),
        }
    }
    (listed, malformed)
}

fn spec_ids() -> BTreeSet<Id> {
    clauses(&spec_text())
        .into_iter()
        .filter(|clause| !is_withdrawn(clause))
        .map(|clause| clause.id)
        .collect()
}

fn list(ids: impl IntoIterator<Item = Id>) -> String {
    ids.into_iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Checks that the plain clause IDs are C1 to Cn with each number exactly
/// once, and that a suffixed ID is defined once and follows a clause that
/// exists and every earlier letter (`C2b` needs `C2` and `C2a`).
fn check_ids(ids: &[Id]) -> Result<(), String> {
    let mut sorted = ids.to_vec();
    sorted.sort_unstable();
    let mut duplicates: Vec<Id> = sorted
        .windows(2)
        .filter(|pair| pair[0] == pair[1])
        .map(|pair| pair[0])
        .collect();
    duplicates.dedup();
    if !duplicates.is_empty() {
        return Err(format!("defined more than once: {}", list(duplicates)));
    }
    let highest = sorted.iter().map(|id| id.number).max().unwrap_or(0);
    let mut missing: Vec<Id> = (1..=highest)
        .map(|number| Id {
            number,
            suffix: None,
        })
        .filter(|id| sorted.binary_search(id).is_err())
        .collect();
    for id in &sorted {
        let Some(last) = id.suffix else { continue };
        missing.extend(
            ('a'..last)
                .map(|suffix| Id {
                    number: id.number,
                    suffix: Some(suffix),
                })
                .filter(|earlier| sorted.binary_search(earlier).is_err()),
        );
    }
    missing.sort_unstable();
    missing.dedup();
    if !missing.is_empty() {
        return Err(format!("missing: {}", list(missing)));
    }
    Ok(())
}

fn ids(text: &[&str]) -> Vec<Id> {
    text.iter()
        .map(|id| clause_id(id).expect("a clause ID"))
        .collect()
}

#[test]
fn clause_ids_take_one_lowercase_suffix_and_no_leading_zero() {
    assert_eq!(
        clause_id("C77a"),
        Some(Id {
            number: 77,
            suffix: Some('a')
        })
    );
    assert_eq!(
        clause_id("C44"),
        Some(Id {
            number: 44,
            suffix: None
        })
    );
    for bad in ["C77A", "C77ab", "C077", "Ca", "C", "S77", "C7a7"] {
        assert_eq!(clause_id(bad), None, "{bad}");
    }
    assert_eq!(clause_id("C77a").unwrap().to_string(), "C77a");
}

#[test]
fn the_id_check_allows_any_order_but_no_gaps_or_repeats() {
    assert_eq!(check_ids(&ids(&["C1", "C2", "C3"])), Ok(()));
    assert_eq!(check_ids(&ids(&["C1", "C2", "C5", "C3", "C4"])), Ok(()));
    assert_eq!(check_ids(&[]), Ok(()));
    assert_eq!(
        check_ids(&ids(&["C1", "C3", "C2", "C3"])),
        Err("defined more than once: C3".to_owned())
    );
    assert_eq!(
        check_ids(&ids(&["C1", "C4", "C2"])),
        Err("missing: C3".to_owned())
    );
    assert_eq!(check_ids(&ids(&["C1", "C2", "C2a", "C3"])), Ok(()));
    assert_eq!(
        check_ids(&ids(&["C1", "C2a", "C2a", "C2"])),
        Err("defined more than once: C2a".to_owned())
    );
    assert_eq!(
        check_ids(&ids(&["C1", "C2a"])),
        Err("missing: C2".to_owned()),
        "an inserted clause follows one that exists"
    );
    assert_eq!(check_ids(&ids(&["C1", "C1b", "C1a", "C2"])), Ok(()));
    assert_eq!(
        check_ids(&ids(&["C1", "C2", "C2b"])),
        Err("missing: C2a".to_owned()),
        "suffixes run a, b, c with none skipped"
    );
    assert_eq!(
        check_ids(&ids(&["C1", "C1a", "C1c", "C1e"])),
        Err("missing: C1b, C1d".to_owned())
    );
}

#[test]
fn clause_text_runs_to_the_next_clause_or_heading() {
    let spec = "## 1\n\n- **C1** One. *Source:* a.\n- **C2** Two:\n\n  | t |\n\n  *Source:* b.\n- **C2a** Inserted. *Source:* c.\n## 2\nText.\n- **G1 Gap.** Not a clause.\n";
    let found = clauses(spec);
    assert_eq!(found.len(), 3);
    assert!(found[1].text.contains("*Source:* b."));
    assert!(
        !found[1].text.contains("Inserted."),
        "an inserted clause is its own"
    );
    assert_eq!(found[2].id.to_string(), "C2a");
    assert!(!found[2].text.contains("Text."));
}

#[test]
fn every_clause_names_a_source_and_the_ids_are_c1_to_cn_once_each() {
    let clauses = clauses(&spec_text());
    assert!(clauses.len() >= 80, "found only {} clauses", clauses.len());
    let ids: Vec<Id> = clauses.iter().map(|clause| clause.id).collect();
    if let Err(problem) = check_ids(&ids) {
        panic!("clause IDs must be C1 to Cn, each once; {problem}");
    }
    let unsourced: Vec<Id> = clauses
        .iter()
        .filter(|clause| !is_withdrawn(clause) && !clause.text.contains("*Source:*"))
        .map(|clause| clause.id)
        .collect();
    assert!(
        unsourced.is_empty(),
        "clauses without a *Source:* line: {}",
        list(unsourced)
    );
}

#[test]
fn covers_lines_are_well_formed_and_name_spec_clauses() {
    let (covered, malformed) = covers();
    assert!(
        malformed.is_empty(),
        "malformed `// covers:` lines (format: `// covers: C20, C26`):\n{}",
        malformed.join("\n")
    );
    let known = spec_ids();
    let unknown: Vec<String> = covered
        .iter()
        .filter(|(id, _)| !known.contains(id))
        .map(|(id, places)| format!("{id} at {}", places.join(", ")))
        .collect();
    assert!(
        unknown.is_empty(),
        "`// covers:` names clauses the spec doesn't have:\n{}",
        unknown.join("\n")
    );
}

#[test]
fn the_cores_cast_spec_lines_are_well_formed_and_name_spec_clauses() {
    let (named, malformed) = core_markers();
    assert!(
        malformed.is_empty(),
        "malformed `// CAST_SPEC:` lines in core/ (format: `// CAST_SPEC: C20, C77a`):\n{}",
        malformed.join("\n")
    );
    assert!(!named.is_empty(), "no `// CAST_SPEC:` line found in core/");
    let known = spec_ids();
    let unknown: Vec<String> = named
        .iter()
        .filter(|(id, _)| !known.contains(id))
        .map(|(id, places)| format!("{id} at {}", places.join(", ")))
        .collect();
    assert!(
        unknown.is_empty(),
        "`// CAST_SPEC:` in core/ names clauses the spec doesn't have:\n{}",
        unknown.join("\n")
    );
}

#[test]
fn the_pending_list_is_well_formed_and_names_spec_clauses() {
    let (pending, malformed) = pending();
    assert!(
        malformed.is_empty(),
        "tests/spec_pending.txt:\n{}",
        malformed.join("\n")
    );
    let known = spec_ids();
    let unknown: Vec<Id> = pending
        .keys()
        .copied()
        .filter(|id| !known.contains(id))
        .collect();
    assert!(
        unknown.is_empty(),
        "pending clauses the spec doesn't have: {}",
        list(unknown)
    );
    let (exempt, _) = exemptions();
    let both: Vec<Id> = pending
        .keys()
        .copied()
        .filter(|id| exempt.contains_key(id))
        .collect();
    assert!(
        both.is_empty(),
        "clauses both exempt and pending (a clause is either the host's or still to be built): {}",
        list(both)
    );
}

#[test]
fn the_exempt_list_is_well_formed_and_names_spec_clauses() {
    let (exempt, malformed) = exemptions();
    assert!(
        malformed.is_empty(),
        "tests/spec_exempt.txt:\n{}",
        malformed.join("\n")
    );
    let known = spec_ids();
    let unknown: Vec<Id> = exempt
        .keys()
        .copied()
        .filter(|id| !known.contains(id))
        .collect();
    assert!(
        unknown.is_empty(),
        "exempt clauses the spec doesn't have: {}",
        list(unknown)
    );
    let repo = crate_dir().join("..");
    let mut broken = Vec::new();
    for (id, reason) in &exempt {
        let tests = host_tests(reason);
        if tests.is_empty() {
            broken.push(format!("{id}: names no `<test> (<path>.rs)`"));
        }
        for (name, path) in tests {
            let defined = fs::read_to_string(repo.join(path)).is_ok_and(|text| {
                text.contains(&format!("fn {name}(")) || text.contains(&format!("fn {name}<"))
            });
            if !defined {
                broken.push(format!("{id}: no `fn {name}` in {path}"));
            }
        }
    }
    assert!(
        broken.is_empty(),
        "exempt clauses whose host test is gone (rename it here too):\n{}",
        broken.join("\n")
    );
}

/// The host tests a reason names as `<test> (<path>.rs)`, the path relative
/// to the repository root.
fn host_tests(reason: &str) -> Vec<(&str, &str)> {
    let mut found = Vec::new();
    let mut before = "";
    for (index, part) in reason.split(" (").enumerate() {
        if index > 0
            && let Some((path, _)) = part.split_once(')')
            && path.ends_with(".rs")
            && let Some(name) = before.split_whitespace().last()
            && !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            found.push((name, path));
        }
        before = part;
    }
    found
}

#[test]
fn an_exempt_reason_names_its_host_tests_with_their_files() {
    assert_eq!(
        host_tests("Cap: its test a_burst (desktop/src/audio/cast.rs) covers it."),
        vec![("a_burst", "desktop/src/audio/cast.rs")]
    );
    assert_eq!(
        host_tests("Guess: its tests one (core/tests/a.rs) and two_2 (desktop/src/b.rs) cover it."),
        vec![("one", "core/tests/a.rs"), ("two_2", "desktop/src/b.rs")]
    );
    assert!(host_tests("Host only (no test named here).").is_empty());
    assert!(host_tests("A Test (core/tests/a.rs).").is_empty());
}

#[test]
fn every_clause_is_covered_exempt_or_pending() {
    let known = spec_ids();
    let (covered, _) = covers();
    let (exempt, _) = exemptions();
    let (pending, _) = pending();
    let missing: Vec<Id> = known
        .iter()
        .copied()
        .filter(|id| {
            !covered.contains_key(id) && !exempt.contains_key(id) && !pending.contains_key(id)
        })
        .collect();
    let stale_exempt: Vec<Id> = exempt
        .keys()
        .copied()
        .filter(|id| covered.contains_key(id))
        .collect();
    let stale_pending: Vec<Id> = pending
        .keys()
        .copied()
        .filter(|id| covered.contains_key(id))
        .collect();
    println!(
        "{} clauses: {} covered, {} exempt, {} pending, {} missing",
        known.len(),
        known.iter().filter(|id| covered.contains_key(id)).count(),
        exempt.len(),
        pending.len(),
        missing.len()
    );
    assert!(
        missing.is_empty() && stale_exempt.is_empty() && stale_pending.is_empty(),
        "clauses neither covered, exempt nor pending (add a `// covers:` test; if only the host applies it, a line in tests/spec_exempt.txt naming the host's test; for a later stage, a line in tests/spec_pending.txt): {}\n\
         exempt clauses that are covered after all (remove them from tests/spec_exempt.txt): {}\n\
         pending clauses that are covered now (remove them from tests/spec_pending.txt): {}",
        list(missing),
        list(stale_exempt),
        list(stale_pending)
    );
}
