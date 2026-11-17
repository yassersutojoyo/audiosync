//! The restore guard of S474: a second copy of the app that waits on the
//! streaming copy and, when that copy ends without restoring, puts back the
//! outputs S469 and S471 moved.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Child;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};

use crate::audio::app_output::AppRecordStore;
use crate::audio::default_output::RecordStore;
use crate::debug_log::DebugLog;
use crate::settings::{AppRecordFile, DefaultRecordFile, SettingsFile};

/// Followed by the streaming copy's process ID.
const GUARD_ARG: &str = "--restore-guard";
/// What the uninstaller passes.
const RESTORE_ARG: &str = "--restore-outputs";
/// How often the guard looks for the record while its parent runs.
const LOOK_EVERY_MS: u32 = 1000;

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Guard(u32),
    Restore,
}

/// What the command line asks of this copy, `None` for a normal launch.
pub fn command(args: impl IntoIterator<Item = OsString>) -> Option<Command> {
    let mut args = args.into_iter().skip(1);
    let first = args.next()?;
    if first == RESTORE_ARG {
        Some(Command::Restore)
    } else if first == GUARD_ARG {
        args.next()?.to_str()?.parse().ok().map(Command::Guard)
    } else {
        None
    }
}

/// Runs `command` with the settings file's records, logging to the debug
/// log the environment asks for.
pub fn run(command: Command) {
    let log = DebugLog::appending_from_env();
    let mut records = SettingsRecords {
        path: SettingsFile::default_path(),
        log: log.clone(),
    };
    match command {
        Command::Guard(parent) => guard(parent, &mut records),
        Command::Restore => records.restore(),
    }
    log.flush(crate::LOG_FLUSH_AT_EXIT);
}

/// For [`crate::audio::controller::Controller::set_restore_guard`]: starts
/// a guard on this process unless the last one still runs.
pub fn keeper() -> Box<dyn FnMut() + Send> {
    let mut running: Option<Child> = None;
    Box::new(move || {
        if running
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
        {
            return;
        }
        running = std::env::current_exe()
            .and_then(|exe| {
                std::process::Command::new(exe)
                    .arg(GUARD_ARG)
                    .arg(std::process::id().to_string())
                    .spawn()
            })
            .ok();
    })
}

/// What the guard reads and does: whether a restore record is on disk, and
/// the restore the next launch would run.
pub trait Records {
    fn on_disk(&mut self) -> bool;
    fn restore(&mut self);
}

/// Waits on process `parent` and restores when it ends while a record is
/// on disk; returns early once no record is.
pub fn guard(parent: u32, records: &mut dyn Records) {
    let parent = Parent::open(parent);
    loop {
        let ended = parent.as_ref().is_none_or(Parent::ended_within_a_look);
        if !records.on_disk() {
            return;
        }
        if ended {
            records.restore();
            return;
        }
    }
}

/// A process handle that can only be waited on.
struct Parent(HANDLE);

impl Parent {
    // A process that already ended and was reaped can't be opened.
    fn open(pid: u32) -> Option<Parent> {
        unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }
            .ok()
            .map(Parent)
    }

    fn ended_within_a_look(&self) -> bool {
        let waited = unsafe { WaitForSingleObject(self.0, LOOK_EVERY_MS) };
        waited != WAIT_TIMEOUT
    }
}

impl Drop for Parent {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

struct SettingsRecords {
    path: Option<PathBuf>,
    log: DebugLog,
}

impl Records for SettingsRecords {
    // A record that can't be read counts as there, so the restore still runs.
    fn on_disk(&mut self) -> bool {
        !matches!(DefaultRecordFile(self.path.clone()).load(), Ok(None))
            || !matches!(AppRecordFile(self.path.clone()).load(), Ok(None))
    }

    fn restore(&mut self) {
        match crate::audio::wasapi::open() {
            Ok(mut audio) => {
                audio.set_debug_log(self.log.clone());
                crate::os::restore_records(&mut audio, &self.log, self.path.as_deref());
            }
            Err(error) => self
                .log
                .log(|| format!("restore guard: the audio backend didn't open: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        present: bool,
        restored: u32,
    }

    impl Records for Fake {
        fn on_disk(&mut self) -> bool {
            self.present
        }

        fn restore(&mut self) {
            self.restored += 1;
        }
    }

    // A short-lived process stands in for AudioSync; its handle stays open
    // in `Child` until the test ends, so the ID can't be reused meanwhile.
    fn ended_parent() -> std::process::Child {
        std::process::Command::new("cmd")
            .args(["/c", "exit"])
            .spawn()
            .expect("cmd starts")
    }

    // covers: S474
    #[test]
    fn the_guard_restores_when_the_parent_ends_with_a_record_on_disk() {
        let mut parent = ended_parent();
        let mut records = Fake {
            present: true,
            ..Fake::default()
        };
        guard(parent.id(), &mut records);
        assert_eq!(records.restored, 1);
        parent.wait().expect("cmd ended");
    }

    // covers: S474
    #[test]
    fn the_guard_does_nothing_when_no_record_is_on_disk() {
        let mut parent = ended_parent();
        let mut records = Fake::default();
        guard(parent.id(), &mut records);
        assert_eq!(records.restored, 0);
        parent.wait().expect("cmd ended");
    }
}
