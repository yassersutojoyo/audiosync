//! Restart Now (spec S303): the new exe starts with a marker naming the
//! quitting process and waits for it before it claims the single instance.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};

pub const MARKER: &str = "--relaunched-from";
pub const WAIT: Duration = Duration::from_secs(10);
/// After [`WAIT`] runs out, the claim is tried this often, this far apart.
const RETRIES: u32 = 10;
const RETRY_STEP: Duration = Duration::from_millis(500);

/// The single-instance claim of a launch, and after a restart whose old
/// process outlived [`WAIT`], a line for the debug log on how it went.
pub struct Claimed<T> {
    pub instance: Option<T>,
    pub note: Option<String>,
}

/// Runs `claim` once; after a restart it first waits for the old process,
/// and if that one outlives the wait, retries the claim for a while: a claim
/// that only handed this launch over to the quitting copy would leave no
/// copy running once it is gone.
pub fn claim_after<T>(
    args: impl IntoIterator<Item = OsString>,
    mut claim: impl FnMut() -> Option<T>,
) -> Claimed<T> {
    let Some(parent) = marked_parent(args) else {
        return Claimed {
            instance: claim(),
            note: None,
        };
    };
    let exited = wait_for_exit(parent, WAIT);
    claim_retrying(parent, exited, claim, std::thread::sleep)
}

fn claim_retrying<T>(
    parent: u32,
    exited: bool,
    mut claim: impl FnMut() -> Option<T>,
    sleep: impl Fn(Duration),
) -> Claimed<T> {
    if exited {
        return Claimed {
            instance: claim(),
            note: None,
        };
    }
    for attempt in 1..=RETRIES {
        if let Some(instance) = claim() {
            return Claimed {
                instance: Some(instance),
                note: Some(format!(
                    "updater: process {parent} still ran after {} s; claimed at try {attempt}",
                    WAIT.as_secs()
                )),
            };
        }
        if attempt < RETRIES {
            sleep(RETRY_STEP);
        }
    }
    Claimed {
        instance: None,
        note: Some(format!(
            "updater: process {parent} still ran after {} s and {RETRIES} claims; exiting",
            WAIT.as_secs()
        )),
    }
}

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

    #[test]
    fn an_old_process_that_outlives_the_wait_gets_the_claim_retried_and_logged() {
        let slept = std::cell::Cell::new(0);
        let sleep = |_| slept.set(slept.get() + 1);

        let mut tries = 0;
        let claimed = claim_retrying(
            7,
            true,
            || {
                tries += 1;
                None::<()>
            },
            sleep,
        );
        assert_eq!(
            (tries, claimed.note),
            (1, None),
            "an ended process: one claim"
        );

        let mut tries = 0;
        let claimed = claim_retrying(
            7,
            false,
            || {
                tries += 1;
                (tries == 3).then_some(())
            },
            sleep,
        );
        assert_eq!(claimed.instance, Some(()));
        assert_eq!(slept.get(), 2);
        assert_eq!(
            claimed.note.as_deref(),
            Some("updater: process 7 still ran after 10 s; claimed at try 3")
        );

        let mut tries = 0;
        let claimed = claim_retrying(
            7,
            false,
            || {
                tries += 1;
                None::<()>
            },
            sleep,
        );
        assert_eq!((tries, claimed.instance), (RETRIES, None));
        assert!(claimed.note.unwrap().ends_with("exiting"));
    }
}
