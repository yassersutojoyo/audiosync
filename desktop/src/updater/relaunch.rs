//! Relaunch Now (spec S303): the new exe starts with a marker naming the
//! quitting process and waits for it before it claims the single instance.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};

pub const MARKER: &str = "--relaunched-from";
pub const WAIT: Duration = Duration::from_secs(10);

/// The process id the marker in `args` names.
pub fn marked_parent(args: impl IntoIterator<Item = OsString>) -> Option<u32> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == MARKER {
            return args.next()?.to_str()?.parse().ok();
        }
    }
    None
}

/// Waits at most `timeout` for process `pid` to end; true once it has (or
/// it can't be opened, which means it is gone).
pub fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    // SAFETY: a plain open of a process id; the handle is closed below.
    let Ok(process) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else {
        return true;
    };
    let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: the handle is open with SYNCHRONIZE access.
    let ended = unsafe { WaitForSingleObject(process, millis) } == WAIT_OBJECT_0;
    // SAFETY: closed once.
    let _ = unsafe { CloseHandle(process) };
    ended
}

/// Starts the installed exe with the marker naming this process.
pub fn start(exe: &Path) -> std::io::Result<()> {
    Command::new(exe)
        .arg(MARKER)
        .arg(std::process::id().to_string())
        .spawn()
        .map(drop)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn relaunch_waits_for_the_old_process_before_claiming_the_instance() {
        let args = ["audiosync.exe", MARKER, "4242"].map(OsString::from);
        assert_eq!(marked_parent(args), Some(4242));
        assert_eq!(marked_parent(["audiosync.exe"].map(OsString::from)), None);
        assert_eq!(marked_parent(["a", MARKER].map(OsString::from)), None);

        let started = Instant::now();
        assert!(!wait_for_exit(
            std::process::id(),
            Duration::from_millis(200)
        ));
        assert!(started.elapsed() >= Duration::from_millis(190));

        let mut child = Command::new("cmd.exe")
            .args(["/c", "exit", "0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(wait_for_exit(pid, WAIT));
        let _ = child.wait();
    }
}
