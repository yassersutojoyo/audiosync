//! Process trees, read from a snapshot of the running processes.
//!
//! Windows records, for each process, the ID of the process that created it.
//! That ID is kept after the parent exits and may then be reused by an
//! unrelated process, so a parent only counts when it was created no later
//! than its child. Creation times that couldn't be read count as a real
//! parent: for the decisions made here, seeing a relationship that isn't
//! there only makes AudioSync refuse a capture it could have made, never
//! capture itself.

use std::collections::{BTreeMap, BTreeSet};

/// The System Idle Process.
pub const IDLE_PID: u32 = 0;

/// The System process, which runs kernel threads.
pub const SYSTEM_PID: u32 = 4;

/// One running process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessEntry {
    /// Its process ID.
    pub pid: u32,
    /// The ID of the process that created it.
    pub parent_pid: u32,
    /// When it was created, in any unit that grows with time (Windows file
    /// time ticks), if it could be read.
    pub created: Option<u64>,
    /// Its executable file name, such as `chrome.exe`.
    pub executable: String,
}

/// A snapshot of the running processes, by process ID.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessTable {
    entries: BTreeMap<u32, ProcessEntry>,
}

impl ProcessTable {
    /// A table of these processes. A process ID listed twice keeps its last
    /// entry.
    pub fn new(entries: impl IntoIterator<Item = ProcessEntry>) -> ProcessTable {
        ProcessTable {
            entries: entries
                .into_iter()
                .map(|entry| (entry.pid, entry))
                .collect(),
        }
    }

    /// The process with this ID.
    pub fn get(&self, pid: u32) -> Option<&ProcessEntry> {
        self.entries.get(&pid)
    }

    /// Whether a process with this ID runs.
    pub fn contains(&self, pid: u32) -> bool {
        self.entries.contains_key(&pid)
    }

    /// How many processes the snapshot holds.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the snapshot holds no process.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The parent of `pid`: the process that created it, if that process
    /// still runs, isn't `pid` itself, and wasn't created after `pid` (which
    /// would make it a later process that got the exited parent's ID).
    pub fn parent(&self, pid: u32) -> Option<u32> {
        let child = self.entries.get(&pid)?;
        let parent = self.entries.get(&child.parent_pid)?;
        if parent.pid == pid {
            return None;
        }
        match (parent.created, child.created) {
            (Some(parent_created), Some(child_created)) if parent_created > child_created => None,
            _ => Some(parent.pid),
        }
    }

    /// The ancestors of `pid`, nearest first. The walk stops at a missing or
    /// reused parent, and at a loop in the recorded parents.
    pub fn ancestors(&self, pid: u32) -> Vec<u32> {
        let mut seen = BTreeSet::from([pid]);
        let mut ancestors = Vec::new();
        let mut current = pid;
        while let Some(parent) = self.parent(current) {
            if !seen.insert(parent) {
                break;
            }
            ancestors.push(parent);
            current = parent;
        }
        ancestors
    }

    /// Whether `pid` is `root` or one of its descendants.
    pub fn in_tree(&self, pid: u32, root: u32) -> bool {
        pid == root || self.ancestors(pid).contains(&root)
    }

    /// The top process of the app `pid` belongs to: the walk goes up while
    /// the parent runs the same executable (compared without regard to
    /// case), so a browser's helper processes lead to its main process.
    pub fn app_root(&self, pid: u32) -> u32 {
        let mut seen = BTreeSet::from([pid]);
        let mut root = pid;
        while let Some(parent) = self.parent(root) {
            let same_app = match (self.entries.get(&parent), self.entries.get(&root)) {
                (Some(parent), Some(child)) => {
                    parent.executable.eq_ignore_ascii_case(&child.executable)
                }
                _ => false,
            };
            if !same_app || !seen.insert(parent) {
                break;
            }
            root = parent;
        }
        root
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A process entry for tests, created at `created` when given.
    pub(crate) fn entry(
        pid: u32,
        parent_pid: u32,
        created: Option<u64>,
        executable: &str,
    ) -> ProcessEntry {
        ProcessEntry {
            pid,
            parent_pid,
            created,
            executable: executable.to_owned(),
        }
    }

    /// explorer (100) started a browser (200) with two helpers (201, 202),
    /// and a terminal (300) that started AudioSync (400), which started a
    /// helper of its own (401).
    pub(crate) fn desktop() -> ProcessTable {
        ProcessTable::new([
            entry(IDLE_PID, IDLE_PID, None, "[System Process]"),
            entry(SYSTEM_PID, IDLE_PID, None, "System"),
            entry(100, 90, Some(10), "explorer.exe"),
            entry(200, 100, Some(20), "browser.exe"),
            entry(201, 200, Some(21), "browser.exe"),
            entry(202, 201, Some(22), "Browser.EXE"),
            entry(300, 100, Some(30), "terminal.exe"),
            entry(400, 300, Some(40), "audiosync.exe"),
            entry(401, 400, Some(41), "helper.exe"),
        ])
    }

    #[test]
    fn a_parent_is_the_listed_creator() {
        let table = desktop();
        assert_eq!(table.parent(201), Some(200));
        assert_eq!(table.parent(100), None, "explorer's creator has exited");
    }

    #[test]
    fn a_parent_created_after_its_child_is_a_reused_id() {
        let table = ProcessTable::new([
            entry(10, 20, Some(5), "app.exe"),
            entry(20, 1, Some(9), "unrelated.exe"),
        ]);
        assert_eq!(table.parent(10), None);
        assert!(table.ancestors(10).is_empty());
    }

    #[test]
    fn an_unknown_creation_time_counts_as_a_real_parent() {
        let table = ProcessTable::new([
            entry(10, 20, Some(5), "app.exe"),
            entry(20, 1, None, "protected.exe"),
        ]);
        assert_eq!(table.parent(10), Some(20));
    }

    #[test]
    fn ancestors_are_listed_nearest_first() {
        assert_eq!(desktop().ancestors(401), vec![400, 300, 100]);
    }

    #[test]
    fn a_loop_in_the_recorded_parents_ends_the_walk() {
        let table = ProcessTable::new([
            entry(10, 20, None, "a.exe"),
            entry(20, 30, None, "b.exe"),
            entry(30, 10, None, "c.exe"),
        ]);
        assert_eq!(table.ancestors(10), vec![20, 30]);
        assert_eq!(table.app_root(10), 10);
    }

    #[test]
    fn a_process_that_names_itself_as_parent_has_none() {
        let table = ProcessTable::new([entry(10, 10, None, "a.exe")]);
        assert_eq!(table.parent(10), None);
    }

    #[test]
    fn a_tree_holds_its_root_and_every_descendant() {
        let table = desktop();
        assert!(table.in_tree(200, 200));
        assert!(table.in_tree(202, 200));
        assert!(table.in_tree(401, 300));
        assert!(!table.in_tree(300, 400));
        assert!(!table.in_tree(200, 300));
    }

    #[test]
    fn an_apps_helpers_lead_to_its_top_process() {
        let table = desktop();
        assert_eq!(table.app_root(202), 200, "the executable name ignores case");
        assert_eq!(table.app_root(200), 200, "explorer runs another executable");
        assert_eq!(table.app_root(401), 401);
    }

    #[test]
    fn a_process_missing_from_the_snapshot_is_its_own_root() {
        let table = desktop();
        assert_eq!(table.app_root(9999), 9999);
        assert!(table.ancestors(9999).is_empty());
    }
}
