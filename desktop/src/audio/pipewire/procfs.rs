//! The running processes, from `/proc`: each one's parent from
//! `/proc/<pid>/stat`, its start time in clock ticks since boot (field 22),
//! and its executable's file name. `init` (pid 1), `kthreadd` (pid 2) and
//! the kernel threads it starts are left out: no app runs as one.

use crate::audio::process::ProcessEntry;
#[cfg(target_os = "linux")]
use crate::audio::process::ProcessTable;

/// The process that starts the kernel threads.
const KTHREADD_PID: u32 = 2;

/// What `/proc/<pid>/stat` holds for one process: its parent and start time,
/// and the command name between the parentheses (at most 15 bytes, so only a
/// fallback for the executable's name). `None` when the text doesn't parse.
pub fn parse_stat(stat: &str) -> Option<(u32, u64, &str)> {
    // The command name may itself hold spaces and parentheses; the last `)`
    // closes it.
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?;
    let mut fields = stat.get(close + 1..)?.split_ascii_whitespace();
    // After the name: state (field 3), ppid (4), then starttime is field 22.
    let parent = fields.nth(1)?.parse().ok()?;
    let started = fields.nth(17)?.parse().ok()?;
    Some((parent, started, comm))
}

/// The entry for process `pid` from its stat text and, when it could be
/// read, the target of `/proc/<pid>/exe`; `None` for a process no app runs
/// as, or a stat that doesn't parse.
pub fn entry(pid: u32, stat: &str, exe: Option<&str>) -> Option<ProcessEntry> {
    let (parent_pid, started, comm) = parse_stat(stat)?;
    if pid <= KTHREADD_PID || parent_pid == KTHREADD_PID {
        return None;
    }
    // A replaced binary reads as "/usr/bin/app (deleted)".
    let executable = exe
        .map(|path| path.strip_suffix(" (deleted)").unwrap_or(path))
        .and_then(|path| path.rsplit('/').next())
        .filter(|name| !name.is_empty())
        .unwrap_or(comm);
    Some(ProcessEntry {
        pid,
        parent_pid,
        created: Some(started),
        executable: executable.to_owned(),
    })
}

/// A snapshot of `/proc`. A process that exits while it is read is left out;
/// only a `/proc` that can't be listed is an error.
#[cfg(target_os = "linux")]
pub fn snapshot() -> std::io::Result<ProcessTable> {
    let mut entries = Vec::new();
    for item in std::fs::read_dir("/proc")? {
        let Ok(item) = item else { continue };
        let Some(pid) = item.file_name().to_str().and_then(|name| name.parse().ok()) else {
            continue;
        };
        let path = item.path();
        let Ok(stat) = std::fs::read_to_string(path.join("stat")) else {
            continue;
        };
        let exe = std::fs::read_link(path.join("exe")).ok();
        let exe = exe.as_deref().and_then(std::path::Path::to_str);
        entries.extend(entry(pid, &stat, exe));
    }
    Ok(ProcessTable::new(entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIREFOX: &str = "4321 (firefox) S 1200 4321 4321 0 -1 4194560 9 0 0 0 \
                           12 3 0 0 20 0 50 0 987654 1024 300 18446744073709551615";

    #[test]
    fn stat_gives_the_parent_and_the_start_time() {
        assert_eq!(parse_stat(FIREFOX), Some((1200, 987_654, "firefox")));
    }

    #[test]
    fn a_name_with_spaces_and_parentheses_ends_at_the_last_parenthesis() {
        let stat = "77 (Web Content (x)) R 4321 77 77 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 555 0 0";
        assert_eq!(parse_stat(stat), Some((4321, 555, "Web Content (x)")));
    }

    #[test]
    fn a_cut_stat_does_not_parse() {
        assert_eq!(parse_stat("4321 (firefox) S 1200 4321"), None);
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn the_executable_is_the_file_name_of_exe() {
        let entry = entry(4321, FIREFOX, Some("/usr/lib/firefox/firefox-bin")).unwrap();
        assert_eq!(entry.executable, "firefox-bin");
        assert_eq!(entry.parent_pid, 1200);
        assert_eq!(entry.created, Some(987_654));
    }

    #[test]
    fn a_replaced_binary_keeps_its_name() {
        let entry = entry(4321, FIREFOX, Some("/usr/bin/spotify (deleted)")).unwrap();
        assert_eq!(entry.executable, "spotify");
    }

    #[test]
    fn without_exe_the_command_name_stands_in() {
        assert_eq!(entry(4321, FIREFOX, None).unwrap().executable, "firefox");
    }

    #[test]
    fn init_and_kernel_threads_are_left_out() {
        let init = "1 (systemd) S 0 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 5 0 0";
        let kthreadd = "2 (kthreadd) S 0 0 0 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 5 0 0";
        let worker = "90 (kworker/0:1) I 2 0 0 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 9 0 0";
        assert_eq!(entry(1, init, None), None);
        assert_eq!(entry(2, kthreadd, None), None);
        assert_eq!(entry(90, worker, None), None);
    }
}
