//! Which MPRIS players belong to the captured app: by process first, a
//! player whose bus connection is the source's process or one of its
//! parents; else by name, a player whose desktop entry or bus name names the
//! source's executable. A browser plays from a child process while its main
//! process owns the player, so the parents count and the children don't.

// Outside Linux only the tests ask.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use crate::audio::now_playing::app_matches;
use crate::now_playing_card::MPRIS_PREFIX;

/// The most parents followed; a real chain is a handful long.
const LINEAGE_LIMIT: usize = 64;

/// The parent process ID in a `/proc/<pid>/stat` line. The command name in
/// parentheses may itself hold spaces and parentheses, so the fields are
/// counted from the last `)`.
pub fn parent_pid(stat: &str) -> Option<u32> {
    let (_, after) = stat.rsplit_once(')')?;
    let mut fields = after.split_whitespace();
    let _state = fields.next()?;
    fields.next()?.parse().ok()
}

/// `pid` and its parents, nearest first, as `stat` reads each process's
/// stat line; the chain ends at a process that can't be read, a parent of 0
/// or a loop.
pub fn lineage(pid: u32, stat: impl Fn(u32) -> Option<String>) -> Vec<u32> {
    let mut chain = vec![pid];
    let mut current = pid;
    while chain.len() < LINEAGE_LIMIT {
        let Some(parent) = stat(current).as_deref().and_then(parent_pid) else {
            break;
        };
        if parent == 0 || chain.contains(&parent) {
            break;
        }
        chain.push(parent);
        current = parent;
    }
    chain
}

/// The stat line of process `pid`.
pub fn proc_stat(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()
}

/// Whether the player on `bus_name`, with the desktop entry it reports,
/// is named for `executable`.
pub fn named_for(bus_name: &str, desktop_entry: Option<&str>, executable: &str) -> bool {
    let player = bus_name
        .strip_prefix(MPRIS_PREFIX)
        .and_then(|rest| rest.split('.').next())
        .unwrap_or_default();
    desktop_entry.is_some_and(|entry| app_matches(entry, executable))
        || app_matches(player, executable)
}

/// Which of `count` players belong to the app: those `by_process` admits,
/// or when it admits none, those `by_name` does. `by_name` is asked only
/// then.
pub fn belonging(
    count: usize,
    by_process: impl Fn(usize) -> bool,
    by_name: impl Fn(usize) -> bool,
) -> Vec<usize> {
    let mine: Vec<usize> = (0..count).filter(|&index| by_process(index)).collect();
    if !mine.is_empty() {
        return mine;
    }
    (0..count).filter(|&index| by_name(index)).collect()
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn the_parent_is_read_past_a_command_name_with_spaces_and_parentheses() {
        assert_eq!(
            parent_pid("4242 (firefox) S 1000 4242 4242 0 -1"),
            Some(1000)
        );
        assert_eq!(parent_pid("77 (Web Content (x)) R 4242 77 77"), Some(4242));
        assert_eq!(parent_pid("77 (cut"), None);
        assert_eq!(parent_pid("77 (odd) S"), None);
    }

    #[test]
    fn the_lineage_runs_up_to_the_first_process() {
        let stat = |pid: u32| {
            let parent = match pid {
                300 => 200,
                200 => 100,
                100 => 1,
                1 => 0,
                _ => return None,
            };
            Some(format!("{pid} (p) S {parent} 0 0"))
        };
        assert_eq!(lineage(300, stat), vec![300, 200, 100, 1]);
        assert_eq!(lineage(9, stat), vec![9], "an unreadable process ends it");
        let looped = |pid: u32| Some(format!("{pid} (p) S {} 0", if pid == 5 { 6 } else { 5 }));
        assert_eq!(lineage(5, looped), vec![5, 6]);
    }

    #[test]
    fn a_player_is_named_for_its_desktop_entry_or_bus_name() {
        let name = "org.mpris.MediaPlayer2.firefox.instance_1_42";
        assert!(named_for(name, None, "firefox"));
        assert!(named_for(
            "org.mpris.MediaPlayer2.chromium",
            Some("chromium"),
            "chromium"
        ));
        assert!(named_for(
            "org.mpris.MediaPlayer2.x",
            Some("spotify"),
            "spotify"
        ));
        assert!(!named_for("org.mpris.MediaPlayer2.vlc", Some("vlc"), "mpv"));
        assert!(!named_for("org.mpris.MediaPlayer2.vlc", None, ""));
    }

    #[test]
    fn players_of_the_process_count_before_those_of_the_name() {
        let named = Cell::new(0);
        let by_name = |index: usize| {
            named.set(named.get() + 1);
            index == 0
        };
        assert_eq!(belonging(3, |index| index == 2, by_name), vec![2]);
        assert_eq!(named.get(), 0, "names aren't asked once a process matched");
        assert_eq!(belonging(3, |_| false, by_name), vec![0]);
        assert_eq!(belonging(3, |_| false, |_| false), Vec::<usize>::new());
    }
}
