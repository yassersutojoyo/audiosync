//! The file operations the updater makes, behind a trait so tests can make
//! one of them fail (a full disk, a refused rename).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use crate::debug_log::DebugLog;

pub trait Sink: Write {
    /// Flushes the data to disk.
    fn sync(&mut self) -> io::Result<()>;
}

impl Sink for File {
    fn sync(&mut self) -> io::Result<()> {
        self.sync_all()
    }
}

pub trait Files: Send + Sync {
    /// A new file; `exclusive` fails when it exists.
    fn create(&self, path: &Path, exclusive: bool) -> io::Result<Box<dyn Sink>>;
    fn remove(&self, path: &Path) -> io::Result<()>;
    /// A rename within the volume that is on disk when it returns.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// [`Files::rename`] that replaces a file `to` names, in one step.
    fn replace(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn open(&self, path: &Path) -> io::Result<File> {
        File::open(path)
    }
}

pub struct RealFiles;

impl Files for RealFiles {
    fn create(&self, path: &Path, exclusive: bool) -> io::Result<Box<dyn Sink>> {
        let mut options = OpenOptions::new();
        options.write(true);
        if exclusive {
            options.create_new(true);
        } else {
            options.create(true).truncate(true);
        }
        Ok(Box::new(options.open(path)?))
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        move_write_through(from, to, false)
    }

    fn replace(&self, from: &Path, to: &Path) -> io::Result<()> {
        move_write_through(from, to, true)
    }
}

fn move_write_through(from: &Path, to: &Path, replace: bool) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;
    let wide =
        |path: &Path| -> Vec<u16> { path.as_os_str().encode_wide().chain(Some(0)).collect() };
    let (from, to) = (wide(from), wide(to));
    // SAFETY: both strings are NUL-terminated and outlive the call.
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            if replace {
                MOVEFILE_WRITE_THROUGH | MOVEFILE_REPLACE_EXISTING
            } else {
                MOVEFILE_WRITE_THROUGH
            },
        )
    }
    .map_err(|error| io::Error::from_raw_os_error(error.code().0 & 0xFFFF))
}

/// Removes `path`, a file that isn't there counting as removed.
pub fn remove_if_there(files: &dyn Files, path: &Path) -> io::Result<()> {
    match files.remove(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Whether `error` says the disk is full.
pub fn is_disk_full(error: &io::Error) -> bool {
    // ERROR_HANDLE_DISK_FULL and ERROR_DISK_FULL.
    error.kind() == io::ErrorKind::StorageFull || matches!(error.raw_os_error(), Some(39 | 112))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

/// Whether `error` says Windows security removed or quarantined the file:
/// ERROR_VIRUS_INFECTED or ERROR_VIRUS_DELETED (spec S315).
pub fn is_blocked(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(225 | 226))
}

/// Whether `error` says another process holds the file: ERROR_SHARING_VIOLATION
/// or ERROR_LOCK_VIOLATION.
pub fn is_locked(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(32 | 33))
}

/// The waits before each retry of a locked file (spec S315): doubling, so a
/// scan that holds the file for a moment passes, and under 8 s in all.
pub const LOCK_RETRIES: [Duration; 5] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

/// What the lock retries may still wait once a quit started, all steps
/// together, so the quit ends inside the time a session end allows.
pub const QUIT_RETRY_BUDGET: Duration = Duration::from_secs(2);

/// The waits between lock retries. A quit ends a running wait at once and
/// gives the retries after it one shared [`QUIT_RETRY_BUDGET`].
pub struct Pause {
    /// Tests count the waits instead of sleeping.
    clock: Option<Arc<dyn Fn(Duration) + Send + Sync>>,
    /// The budget left, once a quit started.
    left: Mutex<Option<Duration>>,
    quit: Condvar,
}

impl Pause {
    pub fn real() -> Pause {
        Pause {
            clock: None,
            left: Mutex::new(None),
            quit: Condvar::new(),
        }
    }

    #[cfg(test)]
    pub fn counted(clock: impl Fn(Duration) + Send + Sync + 'static) -> Pause {
        Pause {
            clock: Some(Arc::new(clock)),
            ..Pause::real()
        }
    }

    pub fn quit(&self) {
        let mut left = self.left.lock().unwrap_or_else(PoisonError::into_inner);
        left.get_or_insert(QUIT_RETRY_BUDGET);
        self.quit.notify_all();
    }

    /// Waits `wait` before a retry; false when the quit budget has no room
    /// for it, so no retry follows.
    fn wait(&self, wait: Duration) -> bool {
        let mut left = self.left.lock().unwrap_or_else(PoisonError::into_inner);
        match (left.as_mut(), &self.clock) {
            (Some(budget), _) => {
                if wait > *budget {
                    return false;
                }
                *budget -= wait;
                drop(left);
                match &self.clock {
                    Some(clock) => clock(wait),
                    None => std::thread::sleep(wait),
                }
            }
            (None, Some(clock)) => {
                drop(left);
                clock(wait);
            }
            (None, None) => {
                let _ = self
                    .quit
                    .wait_timeout_while(left, wait, |left| left.is_none());
            }
        }
        true
    }
}

/// `inner`, with each operation on a locked file tried again after the
/// [`LOCK_RETRIES`] waits; no other error is tried again.
pub struct Retrying<'a> {
    pub inner: &'a dyn Files,
    pub log: &'a DebugLog,
    pub pause: &'a Pause,
}

impl Retrying<'_> {
    fn retried<T>(&self, what: &str, path: &Path, op: impl Fn() -> io::Result<T>) -> io::Result<T> {
        let mut waits = LOCK_RETRIES.iter();
        loop {
            match op() {
                Err(error) if is_locked(&error) => {
                    let code = error.raw_os_error().unwrap_or(-1);
                    let Some(wait) = waits.next() else {
                        self.log.log(|| {
                            format!(
                                "updater: {what} {} still locked (error {code}) after {} retries",
                                file_name(path),
                                LOCK_RETRIES.len()
                            )
                        });
                        return Err(error);
                    };
                    self.log.log(|| {
                        format!(
                            "updater: {what} {} locked (error {code}), again in {} ms",
                            file_name(path),
                            wait.as_millis()
                        )
                    });
                    if !self.pause.wait(*wait) {
                        self.log.log(|| {
                            format!(
                                "updater: {what} {} still locked (error {code}), quitting",
                                file_name(path)
                            )
                        });
                        return Err(error);
                    }
                }
                other => return other,
            }
        }
    }
}

impl Files for Retrying<'_> {
    fn create(&self, path: &Path, exclusive: bool) -> io::Result<Box<dyn Sink>> {
        self.retried("create", path, || self.inner.create(path, exclusive))
    }
    fn remove(&self, path: &Path) -> io::Result<()> {
        self.retried("remove", path, || self.inner.remove(path))
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.retried("rename", from, || self.inner.rename(from, to))
    }
    fn replace(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.retried("replace", from, || self.inner.replace(from, to))
    }
    fn open(&self, path: &Path) -> io::Result<File> {
        self.retried("open", path, || self.inner.open(path))
    }
}
