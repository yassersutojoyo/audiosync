//! The restore guard of S474: a second copy of the app that waits on the
//! streaming copy and, when that copy ends without restoring, puts back the
//! outputs S469 and S471 moved.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Child;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};

use crate::audio::app_output::{AppRecord, AppRecordStore};
use crate::audio::default_output::{Record, RecordStore};
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

/// The per-user sign-in entry of S481, which runs the restore argument.
pub trait SignInEntry: Send {
    fn write(&mut self);
    fn remove(&mut self);
}

/// The restore argument: restores, then removes the sign-in entry unless a
/// record is still on disk, so an entry left without one goes too.
pub fn restore(records: &mut dyn Records, _entry: &mut dyn SignInEntry) {
    records.restore();
}

/// A restore record store that keeps the sign-in entry while any record is
/// on disk (S481): written before a record, removed once none is left.
pub struct WithSignIn<S> {
    store: S,
    entry: Box<dyn SignInEntry>,
    on_disk: Box<dyn FnMut() -> bool + Send>,
}

impl<S> WithSignIn<S> {
    pub fn new(
        store: S,
        entry: Box<dyn SignInEntry>,
        on_disk: Box<dyn FnMut() -> bool + Send>,
    ) -> Self {
        WithSignIn {
            store,
            entry,
            on_disk,
        }
    }
}

impl<S: RecordStore> RecordStore for WithSignIn<S> {
    fn load(&mut self) -> Result<Option<Record>, String> {
        self.store.load()
    }

    fn save(&mut self, record: Option<&Record>) -> Result<(), String> {
        self.store.save(record)
    }
}

impl<S: AppRecordStore> AppRecordStore for WithSignIn<S> {
    fn load(&mut self) -> Result<Option<AppRecord>, String> {
        self.store.load()
    }

    fn save(&mut self, record: Option<&AppRecord>) -> Result<(), String> {
        self.store.save(record)
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
    use std::sync::{Arc, Mutex};

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

    type Log = Arc<Mutex<Vec<&'static str>>>;

    struct Entry(Log);

    impl SignInEntry for Entry {
        fn write(&mut self) {
            self.0.lock().unwrap().push("entry written");
        }

        fn remove(&mut self) {
            self.0.lock().unwrap().push("entry removed");
        }
    }

    struct Store {
        log: Log,
        record: Arc<Mutex<Option<Record>>>,
    }

    impl RecordStore for Store {
        fn load(&mut self) -> Result<Option<Record>, String> {
            Ok(self.record.lock().unwrap().clone())
        }

        fn save(&mut self, record: Option<&Record>) -> Result<(), String> {
            let what = if record.is_some() {
                "record written"
            } else {
                "record deleted"
            };
            self.log.lock().unwrap().push(what);
            *self.record.lock().unwrap() = record.cloned();
            Ok(())
        }
    }

    /// A store whose entry also counts the other kind's record as on disk
    /// when `other` is.
    fn signed_in(other: bool) -> (Log, WithSignIn<Store>) {
        let log = Log::default();
        let record = Arc::new(Mutex::new(None));
        let mine = Arc::clone(&record);
        let store = WithSignIn::new(
            Store {
                log: Arc::clone(&log),
                record,
            },
            Box::new(Entry(Arc::clone(&log))),
            Box::new(move || other || mine.lock().unwrap().is_some()),
        );
        (log, store)
    }

    fn a_record() -> Record {
        Record {
            console: "speakers".to_owned(),
            multimedia: "speakers".to_owned(),
            silent: "cable".to_owned(),
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

    // covers: S481
    #[test]
    fn the_sign_in_entry_is_written_before_the_record_and_removed_after_it() {
        let (log, mut store) = signed_in(false);
        store.save(Some(&a_record())).unwrap();
        store.save(None).unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            [
                "entry written",
                "record written",
                "record deleted",
                "entry removed"
            ]
        );
    }

    // covers: S481
    #[test]
    fn the_sign_in_entry_stays_while_the_other_record_is_on_disk() {
        let (log, mut store) = signed_in(true);
        store.save(Some(&a_record())).unwrap();
        store.save(None).unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            ["entry written", "record written", "record deleted"]
        );
    }

    // covers: S481
    #[test]
    fn the_restore_argument_removes_the_entry_once_no_record_is_left() {
        let log = Log::default();
        let mut restored = Fake::default();
        restore(&mut restored, &mut Entry(Arc::clone(&log)));
        let mut kept = Fake {
            present: true,
            ..Fake::default()
        };
        restore(&mut kept, &mut Entry(Arc::clone(&log)));
        assert_eq!((restored.restored, kept.restored), (1, 1));
        assert_eq!(*log.lock().unwrap(), ["entry removed"]);
    }
}
