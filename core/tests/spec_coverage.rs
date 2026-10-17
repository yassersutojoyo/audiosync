//! Spec coverage: every clause of `docs/ENGINE_SPEC.md` (S1 to Sn) is named by
//! at least one `// covers: S44, S49` line in `core/src` or `core/tests`, or is
//! listed in `core/tests/spec_exempt.txt` with a one-line reason because only the
//! host application can implement it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const MARKER: &str = "// covers:";

fn core_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn spec_text() -> String {
    fs::read_to_string(core_dir().join("../docs/ENGINE_SPEC.md"))
        .expect("docs/ENGINE_SPEC.md is readable")
}

/// A clause ID such as `S44`, as its number.
fn clause_number(id: &str) -> Option<u32> {
    let digits = id.strip_prefix('S')?;
    let first = digits.chars().next()?;
    if first == '0' || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Every clause definition (`- **S12** ...`): its number and its first line.
fn clauses(spec: &str) -> Vec<(u32, String)> {
    spec.lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix("- **")?;
            let (id, _) = rest.split_once("**")?;
            Some((clause_number(id)?, line.to_owned()))
        })
        .collect()
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

/// Clause numbers named by `// covers:` lines, with where each was named; and
/// every malformed marker line.
fn covers() -> (BTreeMap<u32, Vec<String>>, Vec<String>) {
    let mut files = Vec::new();
    rust_files(&core_dir().join("src"), &mut files);
    rust_files(&core_dir().join("tests"), &mut files);
    let mut covered: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    let mut malformed = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).expect("readable source file");
        let name = file
            .strip_prefix(core_dir())
            .unwrap_or(&file)
            .display()
            .to_string();
        for (index, line) in text.lines().enumerate() {
            let Some(list) = line.trim_start().strip_prefix(MARKER) else {
                continue;
            };
            let place = format!("{name}:{}", index + 1);
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

/// The exempt list: clause number to reason; and every malformed line.
fn exemptions() -> (BTreeMap<u32, String>, Vec<String>) {
    let text = fs::read_to_string(core_dir().join("tests/spec_exempt.txt"))
        .expect("tests/spec_exempt.txt is readable");
    let mut exempt = BTreeMap::new();
    let mut malformed = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (id, reason) = line.split_once(' ').unwrap_or((line, ""));
        match clause_number(id) {
            Some(number) if !reason.trim().is_empty() => {
                if exempt.insert(number, reason.trim().to_owned()).is_some() {
                    malformed.push(format!("line {}: {id} is listed twice", index + 1));
                }
            }
            _ => malformed.push(format!(
                "line {}: expected `S<n> <reason>`, got `{line}`",
                index + 1
            )),
        }
    }
    (exempt, malformed)
}

fn spec_numbers() -> BTreeSet<u32> {
    clauses(&spec_text())
        .into_iter()
        .map(|(number, _)| number)
        .collect()
}

fn list(numbers: impl IntoIterator<Item = u32>) -> String {
    numbers
        .into_iter()
        .map(|n| format!("S{n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

// covers: S1
#[test]
fn every_clause_carries_a_tag_and_the_ids_run_from_s1_without_gaps() {
    let clauses = clauses(&spec_text());
    assert!(clauses.len() >= 100, "found only {} clauses", clauses.len());
    let numbers: Vec<u32> = clauses.iter().map(|(number, _)| *number).collect();
    let expected: Vec<u32> = (1..=clauses.len() as u32).collect();
    assert_eq!(
        numbers, expected,
        "clause IDs are S1 to Sn, once each, in order"
    );
    let untagged: Vec<u32> = clauses
        .iter()
        .filter(|(_, line)| !line.contains("*Observed") && !line.contains("*Required"))
        .map(|(number, _)| *number)
        .collect();
    assert!(
        untagged.is_empty(),
        "clauses without an Observed or Required tag: {}",
        list(untagged)
    );
}

#[test]
fn covers_lines_are_well_formed_and_name_spec_clauses() {
    let (covered, malformed) = covers();
    assert!(
        malformed.is_empty(),
        "malformed `// covers:` lines (format: `// covers: S44, S49`):\n{}",
        malformed.join("\n")
    );
    let known = spec_numbers();
    let unknown: Vec<String> = covered
        .iter()
        .filter(|(number, _)| !known.contains(number))
        .map(|(number, places)| format!("S{number} at {}", places.join(", ")))
        .collect();
    assert!(
        unknown.is_empty(),
        "`// covers:` names clauses the spec doesn't have:\n{}",
        unknown.join("\n")
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
    let known = spec_numbers();
    let unknown: Vec<u32> = exempt
        .keys()
        .copied()
        .filter(|number| !known.contains(number))
        .collect();
    assert!(
        unknown.is_empty(),
        "exempt clauses the spec doesn't have: {}",
        list(unknown)
    );
}

#[test]
#[ignore = "enabled after both halves are merged"]
fn every_clause_is_covered_or_exempt() {
    let known = spec_numbers();
    let (covered, _) = covers();
    let (exempt, _) = exemptions();
    let uncovered: Vec<u32> = known
        .iter()
        .copied()
        .filter(|number| !covered.contains_key(number) && !exempt.contains_key(number))
        .collect();
    let stale: Vec<u32> = exempt
        .keys()
        .copied()
        .filter(|number| covered.contains_key(number))
        .collect();
    println!(
        "{} clauses: {} covered, {} exempt, {} uncovered",
        known.len(),
        known
            .iter()
            .filter(|number| covered.contains_key(number))
            .count(),
        exempt.len(),
        uncovered.len()
    );
    assert!(
        uncovered.is_empty() && stale.is_empty(),
        "uncovered clauses (add a `// covers:` test or, if only the host can implement it, a line in tests/spec_exempt.txt): {}\n\
         exempt clauses that are covered after all (remove them from tests/spec_exempt.txt): {}",
        list(uncovered),
        list(stale)
    );
}
