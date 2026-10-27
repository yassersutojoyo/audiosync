//! Spec coverage: every clause of `docs/AIRPLAY_SPEC.md` (P1 to Pn) is named
//! by at least one `// covers: P20, P26` line in `airplay/tests` or in the
//! `#[cfg(test)]` module of a file in `airplay/src`, or is listed in
//! `airplay/tests/spec_pending.txt` with a one-line reason because a later
//! stage implements it (audio and RTP, timing, pairing crypto, real
//! transport, AirPlay 2). A clause is covered only when everything it asks
//! of the sender is implemented and tested; a test for part of a pending
//! clause says so in a plain comment instead of a marker. The pending list
//! may only shrink: a clause that gains a marker must leave it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const MARKER: &str = "// covers:";

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn spec_text() -> String {
    let path = crate_dir().join("..").join("docs").join("AIRPLAY_SPEC.md");
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// A clause ID such as `P44`, as its number.
fn clause_number(id: &str) -> Option<u32> {
    let digits = id.strip_prefix('P')?;
    let first = digits.chars().next()?;
    if first == '0' || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// One clause: its number and its text, from its `- **P12**` line up to the
/// next clause or heading (tables and continuation lines included).
struct Clause {
    number: u32,
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
            .and_then(|(id, _)| clause_number(id));
        if let Some(number) = id {
            found.push(Clause {
                number,
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

/// The line index where a source file's test code starts: its top-level
/// `#[cfg(test)]` module, which ends the file by convention. A `// covers:`
/// line above it would claim coverage for code no test runs. Panics when the
/// first top-level `#[cfg(test)]` doesn't open a `mod`, since a test helper
/// there would let the non-test code below it count.
fn test_code_start(name: &str, text: &str) -> usize {
    let lines: Vec<&str> = text.lines().collect();
    let Some(at) = lines.iter().position(|line| *line == "#[cfg(test)]") else {
        return lines.len();
    };
    let next = lines.get(at + 1).copied().unwrap_or_default();
    assert!(
        next.starts_with("mod "),
        "{name}:{}: the top-level #[cfg(test)] must open the test module, \
         but the next line is `{next}`",
        at + 1
    );
    at
}

/// Clause numbers named by `// covers:` lines, with where each was named; and
/// every malformed marker line, a marker outside test code included.
fn covers() -> (BTreeMap<u32, Vec<String>>, Vec<String>) {
    let mut files = Vec::new();
    rust_files(&crate_dir().join("src"), &mut files);
    rust_files(&crate_dir().join("tests"), &mut files);
    let mut covered: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    let mut malformed = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).expect("readable source file");
        let name = file
            .strip_prefix(crate_dir())
            .unwrap_or(&file)
            .display()
            .to_string();
        let tests_from = if file.starts_with(crate_dir().join("src")) {
            test_code_start(&name, &text)
        } else {
            0
        };
        for (index, line) in text.lines().enumerate() {
            let Some(list) = line.trim_start().strip_prefix(MARKER) else {
                continue;
            };
            let place = format!("{name}:{}", index + 1);
            if index < tests_from {
                malformed.push(format!("{place}: outside test code: {}", line.trim()));
                continue;
            }
            let ids: Vec<Option<u32>> = match list.strip_prefix(' ') {
                Some(list) => list.split(", ").map(clause_number).collect(),
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

/// The pending list: clause number to reason; and every malformed line.
fn pending() -> (BTreeMap<u32, String>, Vec<String>) {
    let text = fs::read_to_string(crate_dir().join("tests").join("spec_pending.txt"))
        .expect("tests/spec_pending.txt is readable");
    let mut pending = BTreeMap::new();
    let mut malformed = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (id, reason) = line.split_once(' ').unwrap_or((line, ""));
        match clause_number(id) {
            Some(number) if !reason.trim().is_empty() => {
                if pending.insert(number, reason.trim().to_owned()).is_some() {
                    malformed.push(format!("line {}: {id} is listed twice", index + 1));
                }
            }
            _ => malformed.push(format!(
                "line {}: expected `P<n> <reason>`, got `{line}`",
                index + 1
            )),
        }
    }
    (pending, malformed)
}

fn spec_numbers() -> BTreeSet<u32> {
    clauses(&spec_text())
        .into_iter()
        .filter(|clause| !is_withdrawn(clause))
        .map(|clause| clause.number)
        .collect()
}

fn list(numbers: impl IntoIterator<Item = u32>) -> String {
    numbers
        .into_iter()
        .map(|n| format!("P{n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Checks that clause IDs are P1 to Pn with each number exactly once.
fn check_ids(numbers: &[u32]) -> Result<(), String> {
    let mut sorted = numbers.to_vec();
    sorted.sort_unstable();
    let mut duplicates: Vec<u32> = sorted
        .windows(2)
        .filter(|pair| pair[0] == pair[1])
        .map(|pair| pair[0])
        .collect();
    duplicates.dedup();
    if !duplicates.is_empty() {
        return Err(format!("defined more than once: {}", list(duplicates)));
    }
    let missing: Vec<u32> = (1..=sorted.last().copied().unwrap_or(0))
        .filter(|number| sorted.binary_search(number).is_err())
        .collect();
    if !missing.is_empty() {
        return Err(format!("missing: {}", list(missing)));
    }
    Ok(())
}

#[test]
fn the_id_check_allows_any_order_but_no_gaps_or_repeats() {
    assert_eq!(check_ids(&[1, 2, 3]), Ok(()));
    assert_eq!(check_ids(&[1, 2, 5, 3, 4]), Ok(()));
    assert_eq!(check_ids(&[]), Ok(()));
    assert_eq!(
        check_ids(&[1, 3, 2, 3]),
        Err("defined more than once: P3".to_owned())
    );
    assert_eq!(check_ids(&[1, 4, 2]), Err("missing: P3".to_owned()));
}

#[test]
fn clause_text_runs_to_the_next_clause_or_heading() {
    let spec = "## 1\n\n- **P1** One. *Source:* a.\n- **P2** Two:\n\n  | t |\n\n  *Source:* b.\n## 2\nText.\n- **G1 Gap.** Not a clause.\n";
    let found = clauses(spec);
    assert_eq!(found.len(), 2);
    assert!(found[1].text.contains("*Source:* b."));
    assert!(!found[1].text.contains("Text."));
}

#[test]
fn every_clause_names_a_source_and_the_ids_are_p1_to_pn_once_each() {
    let clauses = clauses(&spec_text());
    assert!(clauses.len() >= 100, "found only {} clauses", clauses.len());
    let numbers: Vec<u32> = clauses.iter().map(|clause| clause.number).collect();
    if let Err(problem) = check_ids(&numbers) {
        panic!("clause IDs must be P1 to Pn, each once; {problem}");
    }
    let unsourced: Vec<u32> = clauses
        .iter()
        .filter(|clause| !is_withdrawn(clause) && !clause.text.contains("*Source:*"))
        .map(|clause| clause.number)
        .collect();
    assert!(
        unsourced.is_empty(),
        "clauses without a *Source:* line: {}",
        list(unsourced)
    );
}

#[test]
fn only_markers_in_a_source_files_test_module_count() {
    let text = "fn sender() {}\n// covers: P1\n#[cfg(test)]\nmod tests {\n    // covers: P2\n}\n";
    assert_eq!(test_code_start("a.rs", text), 2);
    assert_eq!(
        test_code_start("a.rs", "fn a() {\n    #[cfg(test)]\n    b();\n}\n"),
        4,
        "an indented cfg(test) is not the test module; with none, nothing counts"
    );
    let helper = "#[cfg(test)]\nfn helper() {}\n// covers: P1\nfn sender() {}\n";
    assert!(
        std::panic::catch_unwind(|| test_code_start("a.rs", helper)).is_err(),
        "a top-level cfg(test) that doesn't open a module is not the test module"
    );
}

#[test]
fn covers_lines_are_well_formed_and_name_spec_clauses() {
    let (covered, malformed) = covers();
    assert!(
        malformed.is_empty(),
        "malformed `// covers:` lines (format: `// covers: P20, P26`):\n{}",
        malformed.join("\n")
    );
    let known = spec_numbers();
    let unknown: Vec<String> = covered
        .iter()
        .filter(|(number, _)| !known.contains(number))
        .map(|(number, places)| format!("P{number} at {}", places.join(", ")))
        .collect();
    assert!(
        unknown.is_empty(),
        "`// covers:` names clauses the spec doesn't have:\n{}",
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
    let known = spec_numbers();
    let unknown: Vec<u32> = pending
        .keys()
        .copied()
        .filter(|number| !known.contains(number))
        .collect();
    assert!(
        unknown.is_empty(),
        "pending clauses the spec doesn't have: {}",
        list(unknown)
    );
}

#[test]
fn every_clause_is_covered_or_pending() {
    let known = spec_numbers();
    let (covered, _) = covers();
    let (pending, _) = pending();
    let missing: Vec<u32> = known
        .iter()
        .copied()
        .filter(|number| !covered.contains_key(number) && !pending.contains_key(number))
        .collect();
    let stale: Vec<u32> = pending
        .keys()
        .copied()
        .filter(|number| covered.contains_key(number))
        .collect();
    println!(
        "{} clauses: {} covered, {} pending, {} missing",
        known.len(),
        known
            .iter()
            .filter(|number| covered.contains_key(number))
            .count(),
        pending.len(),
        missing.len()
    );
    assert!(
        missing.is_empty() && stale.is_empty(),
        "clauses neither covered nor pending (add a `// covers:` test, or a line in tests/spec_pending.txt for a later stage): {}\n\
         pending clauses that are covered now (remove them from tests/spec_pending.txt): {}",
        list(missing),
        list(stale)
    );
}
