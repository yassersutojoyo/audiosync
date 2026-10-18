//! Spec coverage: every clause of `docs/ENGINE_SPEC.md` (S1 to Sn, in any
//! document order) is named by at least one `// covers: S44, S49` line in
//! `core/src` or `core/tests`, or is listed in `core/tests/spec_exempt.txt` with
//! a one-line reason because only the host application can implement it, or in
//! `core/tests/spec_pending.txt` with what is missing because the core is meant
//! to implement it but doesn't yet.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const MARKER: &str = "// covers:";

fn core_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn spec_text() -> String {
    let path = core_dir().join("..").join("docs").join("ENGINE_SPEC.md");
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
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
    clause_list("spec_exempt.txt")
}

/// The pending list: clause number to what is missing; and every malformed line.
fn pending() -> (BTreeMap<u32, String>, Vec<String>) {
    clause_list("spec_pending.txt")
}

/// A clause list in `tests/` (`S<n> <reason>` per line, `#` comments): clause
/// number to reason; and every malformed line.
fn clause_list(file: &str) -> (BTreeMap<u32, String>, Vec<String>) {
    let text = fs::read_to_string(core_dir().join("tests").join(file))
        .unwrap_or_else(|error| panic!("tests/{file}: {error}"));
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

/// Checks that clause IDs, taken in document order, are S1 to Sn with each
/// number exactly once. IDs are assigned in order of addition and a later
/// clause sits in the section it belongs to, so document order is free.
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
    assert_eq!(
        check_ids(&[1, 2, 5, 3, 4]),
        Ok(()),
        "a later clause in its section"
    );
    assert_eq!(check_ids(&[]), Ok(()));
    assert_eq!(
        check_ids(&[1, 3, 2, 3]),
        Err("defined more than once: S3".to_owned())
    );
    assert_eq!(check_ids(&[1, 4, 2]), Err("missing: S3".to_owned()));
    assert_eq!(check_ids(&[2, 3]), Err("missing: S1".to_owned()));
}

// covers: S1
#[test]
fn every_clause_carries_a_tag_and_the_ids_are_s1_to_sn_once_each() {
    let clauses = clauses(&spec_text());
    assert!(clauses.len() >= 100, "found only {} clauses", clauses.len());
    let numbers: Vec<u32> = clauses.iter().map(|(number, _)| *number).collect();
    if let Err(problem) = check_ids(&numbers) {
        panic!("clause IDs must be S1 to Sn, each once, in any order; {problem}");
    }
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
    let (exempt, _) = exemptions();
    let both: Vec<u32> = pending
        .keys()
        .copied()
        .filter(|number| exempt.contains_key(number))
        .collect();
    assert!(
        both.is_empty(),
        "clauses both exempt and pending (a clause is either host-only or still to be built): {}",
        list(both)
    );
}

#[test]
fn every_clause_is_covered_exempt_or_pending() {
    let known = spec_numbers();
    let (covered, _) = covers();
    let (exempt, _) = exemptions();
    let (pending, _) = pending();
    let uncovered: Vec<u32> = known
        .iter()
        .copied()
        .filter(|number| {
            !covered.contains_key(number)
                && !exempt.contains_key(number)
                && !pending.contains_key(number)
        })
        .collect();
    let stale_exempt: Vec<u32> = exempt
        .keys()
        .copied()
        .filter(|number| covered.contains_key(number))
        .collect();
    let stale_pending: Vec<u32> = pending
        .keys()
        .copied()
        .filter(|number| covered.contains_key(number))
        .collect();
    println!(
        "{} clauses: {} covered, {} exempt, {} pending, {} uncovered",
        known.len(),
        known
            .iter()
            .filter(|number| covered.contains_key(number))
            .count(),
        exempt.len(),
        pending.len(),
        uncovered.len()
    );
    assert!(
        uncovered.is_empty() && stale_exempt.is_empty() && stale_pending.is_empty(),
        "uncovered clauses (add a `// covers:` test; if only the host can implement it, a line in tests/spec_exempt.txt; \
         if the core doesn't implement it yet, a line in tests/spec_pending.txt): {}\n\
         exempt clauses that are covered after all (remove them from tests/spec_exempt.txt): {}\n\
         pending clauses that are covered now (remove them from tests/spec_pending.txt): {}",
        list(uncovered),
        list(stale_exempt),
        list(stale_pending)
    );
}
