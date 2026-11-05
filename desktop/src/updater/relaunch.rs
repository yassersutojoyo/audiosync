//! Restart Now (spec S303, S314): the new exe starts with a marker naming
//! the quitting process, signals it that it started, and waits for it
//! before it claims the single instance.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

#[cfg(windows)]
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
#[cfg(windows)]
use windows::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, OpenEventW, OpenProcess, PROCESS_SYNCHRONIZE, ResetEvent,
    SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};
#[cfg(windows)]
use windows::core::PCWSTR;

pub const MARKER: &str = "--relaunched-from";
pub const WAIT: Duration = crate::single_instance::RELAUNCH_WAIT;
/// How long the old process waits for the new one's start signal: a first
/// start from a cold disk plus Defender's scan of a new file (spec S314).
pub const START_WAIT: Duration = Duration::from_secs(15);
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
    claim: impl FnMut() -> Option<T>,
) -> Claimed<T> {
    let legacy_note = std::cell::Cell::<Option<String>>::new(None);
    let signal = |old| {
        let made = signal_started(old);
        #[cfg(not(windows))]
        legacy_note.set(signal_legacy(old));
        made
    };
    let mut claimed = claim_after_with(args, signal, wait_for_exit, claim, std::thread::sleep);
    if let Some(line) = legacy_note.take() {
        claimed.note = Some(match claimed.note {
            Some(note) => format!("{note}; {line}"),
            None => line,
        });
    }
    claimed
}

fn claim_after_with<T>(
    args: impl IntoIterator<Item = OsString>,
    signal: impl FnOnce(u32) -> bool,
    wait: impl FnOnce(u32, Duration) -> bool,
    mut claim: impl FnMut() -> Option<T>,
    sleep: impl Fn(Duration),
) -> Claimed<T> {
    let Some(parent) = marked_parent(args) else {
        return Claimed {
            instance: claim(),
            note: None,
        };
    };
    signal(parent);
    let exited = wait(parent, WAIT);
    claim_retrying(parent, exited, claim, sleep)
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
#[cfg(windows)]
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

/// Waits at most `timeout` for process `pid` to end; true once it has, or
/// only its zombie is left for its parent to reap.
#[cfg(not(windows))]
pub fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let gone = || match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(')')
            .is_none_or(|(_, rest)| matches!(rest.trim_start().chars().next(), Some('Z' | 'X'))),
    };
    let deadline = Instant::now() + timeout;
    while !gone() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

/// The name of the start signal of a new instance whose old process is `old`.
#[cfg(windows)]
pub fn signal_name(old: u32) -> String {
    format!("Local\\AudioSync-update-started-{old}")
}

/// Linux: the start signal of a new instance whose old process is `old`, a
/// file in `dir` (spec S424).
#[cfg_attr(windows, allow(dead_code))]
pub fn signal_file(dir: &Path, old: u32) -> PathBuf {
    dir.join(format!("AudioSync-update-started-{old}"))
}

/// Linux: the start signal's folder, `runtime` (the user's private runtime
/// folder) when there is one, else the instance lock's `audiosync-<user>` in
/// the temp folder, never the shared temp folder itself (spec S424).
#[cfg_attr(windows, allow(dead_code))]
pub fn signal_dir_in(runtime: Option<PathBuf>, temp: &Path, user: u32) -> PathBuf {
    runtime.unwrap_or_else(|| temp.join(format!("audiosync-{user}")))
}

/// Linux: the folder 0.8.7 and older wait for the start signal in
/// (`$XDG_RUNTIME_DIR` as set, else `temp`), when it is not `private`.
#[cfg_attr(windows, allow(dead_code))]
pub fn legacy_signal_dir(
    private: Option<&Path>,
    runtime_var: Option<OsString>,
    temp: &Path,
) -> Option<PathBuf> {
    let legacy = runtime_var.map_or_else(|| temp.to_path_buf(), PathBuf::from);
    (private != Some(legacy.as_path())).then_some(legacy)
}

/// [`signal_dir_in`], created 0700 and refused when another account owns it.
#[cfg(not(windows))]
fn signal_dir() -> io::Result<PathBuf> {
    use crate::os::xdg;
    let user = xdg::user_id().ok_or_else(|| io::Error::other("this process's user is unknown"))?;
    let runtime = xdg::runtime_base(std::env::var_os("XDG_RUNTIME_DIR"), user);
    let dir = signal_dir_in(runtime, &std::env::temp_dir(), user);
    xdg::owned_private_dir(&dir, user)?;
    Ok(dir)
}

/// Makes the start signal the old process `old` waits for; false when that
/// process is gone.
#[cfg(not(windows))]
pub fn signal_started(old: u32) -> bool {
    !wait_for_exit(old, Duration::ZERO)
        && signal_dir().is_ok_and(|dir| make_signal(&signal_file(&dir, old)))
}

/// Also makes the start signal where 0.8.7 and older wait for it, best
/// effort; a line for the debug log when that fails (spec S424).
// Remove once no supported version waits outside the private folder.
#[cfg(not(windows))]
fn signal_legacy(old: u32) -> Option<String> {
    let private = signal_dir().ok();
    let runtime = std::env::var_os("XDG_RUNTIME_DIR");
    let dir = legacy_signal_dir(private.as_deref(), runtime, &std::env::temp_dir())?;
    let signal = signal_file(&dir, old);
    (!make_signal(&signal)).then(|| {
        format!(
            "updater: no start signal for older versions at {}",
            signal.display()
        )
    })
}

/// Creates `signal` as a new file only the user can read; never through a
/// link or over a file already there.
#[cfg(not(windows))]
fn make_signal(signal: &Path) -> bool {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(signal)
        .is_ok()
}

/// Sets the start signal the old process `old` waits on; false when there
/// is none (that process is gone).
#[cfg(windows)]
pub fn signal_started(old: u32) -> bool {
    let name = wide(&signal_name(old));
    // SAFETY: a NUL-terminated name that outlives the call.
    let Ok(event) = (unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(name.as_ptr())) })
    else {
        return false;
    };
    // SAFETY: the handle is open with EVENT_MODIFY_STATE access.
    let set = unsafe { SetEvent(event) }.is_ok();
    // SAFETY: closed once.
    let _ = unsafe { CloseHandle(event) };
    set
}

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// How the new version's first start went.
#[derive(Debug)]
pub enum Start {
    Signalled {
        pid: u32,
    },
    Exited {
        pid: u32,
        code: Option<i32>,
    },
    /// No signal in time; the new process was ended.
    TimedOut {
        pid: u32,
    },
    Failed(io::Error),
}

/// Starts the new exe and waits for its start signal.
pub trait Launch: Send + Sync {
    fn start(&self, exe: &Path, wait: Duration) -> Start;
}

/// The installed exe as a child of this process, with the marker naming it.
pub struct ProcessLaunch;

impl Launch for ProcessLaunch {
    fn start(&self, exe: &Path, wait: Duration) -> Start {
        let own = std::process::id();
        let mut command = Command::new(exe);
        command.arg(MARKER).arg(own.to_string());
        start_and_wait(&mut command, own, wait)
    }
}

/// Linux: [`start_and_watch`] for the signal file named after `own`.
#[cfg(not(windows))]
pub fn start_and_wait(command: &mut Command, own: u32, wait: Duration) -> Start {
    match signal_dir() {
        Ok(dir) => start_and_watch(command, &signal_file(&dir, own), wait),
        Err(error) => Start::Failed(error),
    }
}

/// Runs `command` as a child process and waits up to `wait` for the file
/// `signal` to appear, which it takes; a child that neither makes it nor
/// ends in time is ended. A file left from an earlier try is removed first.
#[cfg_attr(windows, allow(dead_code))]
pub fn start_and_watch(command: &mut Command, signal: &Path, wait: Duration) -> Start {
    let _ = std::fs::remove_file(signal);
    let mut child = match command.stdin(Stdio::null()).spawn() {
        Ok(child) => child,
        Err(error) => return Start::Failed(error),
    };
    let pid = child.id();
    let deadline = Instant::now() + wait;
    loop {
        if std::fs::remove_file(signal).is_ok() {
            return Start::Signalled { pid };
        }
        if let Ok(Some(status)) = child.try_wait() {
            // A child that signals and then ends counts as started.
            if std::fs::remove_file(signal).is_ok() {
                return Start::Signalled { pid };
            }
            return Start::Exited {
                pid,
                code: status.code(),
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Start::TimedOut { pid };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Runs `command` as a child process, never through the shell, and waits up
/// to `wait` for the start signal named after `own`; a child that neither
/// signals nor ends in time is ended.
#[cfg(windows)]
pub fn start_and_wait(command: &mut Command, own: u32, wait: Duration) -> Start {
    let name = wide(&signal_name(own));
    // SAFETY: a NUL-terminated name that outlives the call.
    let event = match unsafe { CreateEventW(None, true, false, PCWSTR(name.as_ptr())) } {
        Ok(event) => event,
        Err(error) => return Start::Failed(io::Error::from_raw_os_error(error.code().0 & 0xFFFF)),
    };
    // An event an earlier try left set would read as this start's signal.
    // SAFETY: the handle is open; CreateEventW grants full access.
    let _ = unsafe { ResetEvent(event) };
    let start = match command.stdin(Stdio::null()).spawn() {
        Err(error) => Start::Failed(error),
        Ok(mut child) => {
            let pid = child.id();
            let process = HANDLE(child.as_raw_handle());
            let millis = u32::try_from(wait.as_millis()).unwrap_or(u32::MAX);
            // SAFETY: both handles stay open for the call; the event comes
            // first, so a child that signals and then ends counts as started.
            let woke = unsafe { WaitForMultipleObjects(&[event, process], false, millis) };
            if woke == WAIT_OBJECT_0 {
                Start::Signalled { pid }
            } else if woke.0 == WAIT_OBJECT_0.0 + 1 {
                let code = child
                    .try_wait()
                    .ok()
                    .flatten()
                    .and_then(|status| status.code());
                Start::Exited { pid, code }
            } else {
                let _ = child.kill();
                let _ = child.wait();
                Start::TimedOut { pid }
            }
        }
    };
    // SAFETY: closed once.
    let _ = unsafe { CloseHandle(event) };
    start
}

#[cfg(all(test, windows))]
pub mod tests {
    use std::time::Instant;

    use super::*;

    // covers: S303
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

        // The child ends 1 s after WAIT (ping counts whole seconds), as a
        // plain cmd.exe did under heavy parallel test load.
        let past_wait = u32::try_from(WAIT.as_secs()).unwrap() + 1;
        let mut child = sleeper(past_wait).spawn().unwrap();
        let pid = child.id();
        assert!(wait_for_exit(pid, Duration::from_secs(60)));
        let _ = child.wait();
    }

    // covers: S303
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

    // covers: S303, S314
    #[test]
    fn the_new_instance_signals_before_it_reads_the_settings_or_claims_the_instance() {
        let args = || ["audiosync.exe", MARKER, "4242"].map(OsString::from);
        for old_ends in [true, false] {
            let steps = std::cell::RefCell::new(Vec::new());
            let claimed = claim_after_with(
                args(),
                |old| {
                    steps.borrow_mut().push(format!("signal {old}"));
                    true
                },
                |old, wait| {
                    steps
                        .borrow_mut()
                        .push(format!("wait {old} {} s", wait.as_secs()));
                    old_ends
                },
                || {
                    // The settings file is read once the instance is claimed.
                    steps.borrow_mut().push("claim".to_owned());
                    Some(())
                },
                |_| {},
            );
            assert_eq!(claimed.instance, Some(()));
            assert_eq!(
                *steps.borrow(),
                ["signal 4242", "wait 4242 10 s", "claim"],
                "the old process ends: {old_ends}"
            );
        }
        let steps = std::cell::RefCell::new(Vec::new());
        let _ = claim_after_with(
            ["audiosync.exe"].map(OsString::from),
            |_| {
                steps.borrow_mut().push("signal");
                true
            },
            |_, _| {
                steps.borrow_mut().push("wait");
                true
            },
            || {
                steps.borrow_mut().push("claim");
                Some(())
            },
            |_| {},
        );
        assert_eq!(*steps.borrow(), ["claim"], "a plain start signals nothing");
    }

    /// A child that runs for about `seconds` and never signals.
    pub fn sleeper(seconds: u32) -> Command {
        let mut command = Command::new("ping.exe");
        command
            .args(["-n", &(seconds + 1).to_string(), "127.0.0.1"])
            .stdout(Stdio::null());
        command
    }

    /// A start signal named after a made-up old process, so parallel tests
    /// don't share one.
    pub fn made_up_old(n: u32) -> u32 {
        0xA000_0000 + (std::process::id() & 0xFFFF) * 16 + n
    }

    // covers: S314
    #[test]
    fn the_start_signal_reaches_the_old_process_and_a_silent_child_is_ended() {
        let old = made_up_old(1);
        assert!(!signal_started(old), "no old process waits");
        let signaller = std::thread::spawn(move || {
            for _ in 0..100 {
                if signal_started(old) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        });
        let started = Instant::now();
        let start = start_and_wait(&mut sleeper(3), old, Duration::from_secs(10));
        assert!(signaller.join().unwrap());
        assert!(matches!(start, Start::Signalled { .. }), "{start:?}");
        assert!(started.elapsed() < Duration::from_secs(3));

        let mut exits = Command::new("cmd.exe");
        // Exits 1 s after WAIT, as late as a plain `exit 3` did under heavy
        // parallel test load.
        let late_exit = format!("ping -n {} 127.0.0.1 >nul & exit 3", WAIT.as_secs() + 2);
        exits.args(["/c", &late_exit]);
        let start = start_and_wait(&mut exits, made_up_old(2), Duration::from_secs(60));
        assert!(
            matches!(start, Start::Exited { code: Some(3), .. }),
            "{start:?}"
        );

        let started = Instant::now();
        let start = start_and_wait(&mut sleeper(30), made_up_old(3), Duration::from_millis(300));
        let Start::TimedOut { pid } = start else {
            panic!("{start:?}");
        };
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            wait_for_exit(pid, Duration::from_secs(2)),
            "the child was ended"
        );
    }

    /// A command that runs `script` through the platform's shell.
    fn shell(script: &str) -> Command {
        #[cfg(windows)]
        let command = {
            use std::os::windows::process::CommandExt;
            // cmd.exe reads its own quotes, not the escaped ones `arg` writes.
            let mut command = Command::new("cmd.exe");
            command.arg("/c").raw_arg(script);
            command
        };
        #[cfg(not(windows))]
        let command = {
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            command
        };
        command
    }

    // covers: S424
    #[test]
    fn the_linux_start_signal_is_a_file_the_new_instance_makes() {
        let dir = crate::updater::tests::test_dir("file-signal");
        let signal = signal_file(&dir, 4242);
        assert_eq!(signal.file_name().unwrap(), "AudioSync-update-started-4242");
        // A file an earlier try left is no signal of this start.
        std::fs::write(&signal, b"").unwrap();
        let start = start_and_watch(&mut shell("exit 3"), &signal, Duration::from_secs(10));
        assert!(
            matches!(start, Start::Exited { code: Some(3), .. }),
            "{start:?}"
        );

        let make = if cfg!(windows) {
            format!(
                "type nul > \"{}\" & ping -n 3 127.0.0.1 >nul",
                signal.display()
            )
        } else {
            format!(": > '{}'; sleep 2", signal.display())
        };
        let start = start_and_watch(&mut shell(&make), &signal, Duration::from_secs(10));
        assert!(matches!(start, Start::Signalled { .. }), "{start:?}");
        assert!(!signal.exists(), "the signal is taken");

        let started = Instant::now();
        let quiet = if cfg!(windows) {
            "ping -n 30 127.0.0.1 >nul"
        } else {
            "sleep 30"
        };
        let start = start_and_watch(&mut shell(quiet), &signal, Duration::from_millis(300));
        assert!(matches!(start, Start::TimedOut { .. }), "{start:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    // covers: S424
    #[test]
    fn without_a_private_runtime_folder_the_signal_goes_in_the_users_own_folder() {
        let temp = Path::new("/tmp");
        let runtime = PathBuf::from("/run/user/1000");
        assert_eq!(signal_dir_in(Some(runtime.clone()), temp, 1000), runtime);
        // Another account can make files in the shared temp folder and so
        // block the start signal, which rolls every update back.
        assert_eq!(signal_dir_in(None, temp, 1000), temp.join("audiosync-1000"));
    }

    // covers: S424
    #[test]
    fn the_new_instance_also_signals_where_older_versions_wait() {
        let temp = Path::new("/tmp");
        let private = signal_dir_in(None, temp, 1000);
        assert_eq!(
            legacy_signal_dir(Some(&private), None, temp).as_deref(),
            Some(temp)
        );
        let shared = OsString::from("/run/shared");
        assert_eq!(
            legacy_signal_dir(Some(&private), Some(shared), temp).as_deref(),
            Some(Path::new("/run/shared"))
        );
        // A private runtime folder is where both versions wait: one signal.
        let runtime = PathBuf::from("/run/user/1000");
        let private = signal_dir_in(Some(runtime.clone()), temp, 1000);
        assert_eq!(private, runtime);
        assert_eq!(
            legacy_signal_dir(Some(&private), Some(runtime.into()), temp),
            None
        );
        assert_eq!(legacy_signal_dir(None, None, temp).as_deref(), Some(temp));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("audiosync-relaunch-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // covers: S424
    #[test]
    fn the_signal_file_is_new_private_and_never_follows_a_link() {
        let dir = test_dir("signal");
        let signal = signal_file(&dir, 4242);
        assert!(make_signal(&signal));
        let mode = std::fs::metadata(&signal).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        // A file already there, or a link someone put there, is no signal.
        assert!(!make_signal(&signal));
        std::fs::remove_file(&signal).unwrap();
        let target = dir.join("target");
        std::fs::write(&target, b"kept").unwrap();
        std::os::unix::fs::symlink(&target, &signal).unwrap();
        assert!(!make_signal(&signal));
        assert_eq!(std::fs::read(&target).unwrap(), b"kept");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // covers: S424
    #[test]
    fn a_new_instance_that_makes_the_file_counts_as_started() {
        let dir = test_dir("watch");
        let signal = signal_file(&dir, 4242);
        let mut command = Command::new("sh");
        command.args(["-c", &format!(": > '{}'; sleep 2", signal.display())]);
        let start = start_and_watch(&mut command, &signal, Duration::from_secs(10));
        assert!(matches!(start, Start::Signalled { .. }), "{start:?}");
        assert!(!signal.exists(), "the signal is taken");
        let mut command = Command::new("sh");
        command.args(["-c", "exit 3"]);
        let start = start_and_watch(&mut command, &signal, Duration::from_secs(10));
        let Start::Exited { pid, code: Some(3) } = &start else {
            panic!("{start:?}");
        };
        assert!(
            wait_for_exit(*pid, Duration::from_secs(2)),
            "the child ended"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
