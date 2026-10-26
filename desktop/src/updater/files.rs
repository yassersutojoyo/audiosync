//! The file operations the updater makes, behind a trait so tests can make
//! one of them fail (a full disk, a refused rename).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
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
        rename_write_through(from, to)
    }
}

fn rename_write_through(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
    use windows::core::PCWSTR;
    let wide =
        |path: &Path| -> Vec<u16> { path.as_os_str().encode_wide().chain(Some(0)).collect() };
    let (from, to) = (wide(from), wide(to));
    // SAFETY: both strings are NUL-terminated and outlive the call.
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_WRITE_THROUGH,
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

/// `inner`, with each operation on a locked file tried again after the
/// [`LOCK_RETRIES`] waits; no other error is tried again.
pub struct Retrying<'a> {
    pub inner: &'a dyn Files,
    pub log: &'a DebugLog,
}

impl Retrying<'_> {
    fn retried<T>(&self, what: &str, path: &Path, op: impl Fn() -> io::Result<T>) -> io::Result<T> {
        let _ = (what, path);
        op()
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
    fn open(&self, path: &Path) -> io::Result<File> {
        self.retried("open", path, || self.inner.open(path))
    }
}
