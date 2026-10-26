//! The file operations the updater makes, behind a trait so tests can make
//! one of them fail (a full disk, a refused rename).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

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
