//! The app's data folder, `%LOCALAPPDATA%\AudioSync`, and how files in it
//! are written.
//!
//! The folder holds the paired Apple TVs' credentials
//! (`crate::audio::pairings`) and the user's settings (`crate::settings`).
//! Each file is replaced as a whole: written to a temporary file next to it,
//! flushed to disk, then renamed over it, so a crash or a power cut leaves
//! the old file or the new one, never half of one.
//!
//! A file is read at launch with one more try after a short pause when the
//! first read fails for another reason than the file not being there
//! ([`read_at_launch`]): an antivirus scan or a backup tool may hold it for a
//! moment. A file that still can't be read, or that turns out damaged, is
//! never saved over unseen: each store keeps it aside first, in a copy with
//! a name of its own that nothing replaces or removes later ([`keep_aside`]),
//! and refuses to save while it can't be read.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The app's data folder under `%LOCALAPPDATA%`.
pub const FOLDER_NAME: &str = "AudioSync";

/// How long a read that failed waits before its second try.
pub const READ_RETRY_PAUSE: Duration = Duration::from_millis(500);

/// A file ignored at launch that couldn't be kept aside then (it still
/// couldn't be read) is tried again this many polls later: about every ten
/// seconds at the UI's two polls a second, so the user learns about it
/// without changing anything first.
pub const KEEP_ASIDE_RETRY_POLLS: u32 = 20;

/// The file at `path`, read once more after [`READ_RETRY_PAUSE`] when the
/// first read fails for another reason than the file not being there.
/// Blocks for that pause, so it is for launch, not for the UI's polls.
pub fn read_at_launch(path: &Path) -> io::Result<Vec<u8>> {
    match fs::read(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            std::thread::sleep(READ_RETRY_PAUSE);
            fs::read(path)
        }
        read => read,
    }
}

/// Copies the file at `path` aside, next to it, under a name of its own
/// made from `name` ([`keep_bytes_aside`]), and returns the copy's path;
/// `None` when there is no file at `path`. A file that can't be read is an
/// error: it is then not kept, so the caller must not replace it.
pub fn keep_aside(path: &Path, name: &str) -> io::Result<Option<PathBuf>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    keep_bytes_aside(path, name, &bytes).map(Some)
}

/// How many names a copy kept aside tries within one second before it
/// gives up.
const MAX_COPIES_PER_SECOND: u32 = 100;

/// Keeps `bytes`, what the file at `path` holds, in a new file next to it
/// and returns that copy's path. The copy is named after `name` and the
/// time in UTC: `settings.ignored.json` gives
/// `settings.ignored-2026-10-20T051230Z.json`, with `-2`, `-3` and so on
/// after the time when that name is taken.
///
/// A copy is never replaced and never removed here, so every file kept
/// aside stays, whatever later launches find; when one of the copies named
/// after `name` holds the same bytes already, its path is returned and no
/// new copy is made. The copy is written and flushed to disk before this
/// returns, so the caller may then replace the file at `path`.
pub fn keep_bytes_aside(path: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let name = Path::new(name);
    let stem = name.file_stem().unwrap_or_default().to_string_lossy();
    let extension = name
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    let prefix = format!("{stem}-");
    if let Some(copy) = same_copy(path, &prefix, &extension, bytes) {
        return Ok(copy);
    }
    let stamp = file_stamp(SystemTime::now());
    for number in 1..=MAX_COPIES_PER_SECOND {
        let copy = if number == 1 {
            path.with_file_name(format!("{prefix}{stamp}{extension}"))
        } else {
            path.with_file_name(format!("{prefix}{stamp}-{number}{extension}"))
        };
        match write_new(&copy, bytes) {
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            written => return written.map(|()| copy),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "every name for a copy kept aside this second is taken",
    ))
}

/// A copy next to `path` whose name starts with `prefix` and ends with
/// `extension` and which holds `bytes`, if there is one.
fn same_copy(path: &Path, prefix: &str, extension: &str, bytes: &[u8]) -> Option<PathBuf> {
    let folder = path
        .parent()
        .filter(|folder| !folder.as_os_str().is_empty())?;
    fs::read_dir(folder).ok()?.flatten().find_map(|entry| {
        let file_name = entry.file_name();
        let file_name = file_name.to_str()?;
        let named = file_name.starts_with(prefix) && file_name.ends_with(extension);
        let same_size = entry
            .metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() == bytes.len() as u64);
        let copy = entry.path();
        (named && same_size && fs::read(&copy).is_ok_and(|kept| kept == bytes)).then_some(copy)
    })
}

/// `time` in UTC to the second, for a file name: `2026-10-20T051230Z`.
fn file_stamp(time: SystemTime) -> String {
    let stamp = crate::debug_log::utc_timestamp(time);
    let to_the_second = stamp
        .split_once('.')
        .map_or(stamp.as_str(), |(head, _)| head);
    format!("{}Z", to_the_second.replace(':', ""))
}

/// Writes `bytes` to a new file at `path`, flushed to disk; an error with
/// [`io::ErrorKind::AlreadyExists`] when something is there already, which
/// is left alone. A file this made and couldn't fill is removed.
fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    if written.is_err() {
        drop(file);
        let _ = fs::remove_file(path);
    }
    written
}

/// The app's data folder, `%LOCALAPPDATA%\AudioSync`, or `None` when the
/// system names no folder for it (`crate::os::data_base`). The folder may
/// not exist yet.
pub fn folder() -> Option<PathBuf> {
    crate::os::data_base().map(|base| base.join(FOLDER_NAME))
}

/// The temporary file `path` is written through: the same name with this
/// process's ID and the extension `tmp` (`settings.1234.tmp`), in the same
/// folder, so the rename never crosses volumes and two copies of the app
/// never write through the same file.
pub fn temporary_path(path: &Path) -> PathBuf {
    path.with_extension(format!("{}.tmp", std::process::id()))
}

/// Replaces the file at `path` with `bytes`, creating its folder if needed.
/// The bytes go to a temporary file, flushed to disk before it is renamed
/// over `path`; a temporary file left by a failed write is removed.
///
/// A leftover at the temporary path (from a crash, or a link someone put
/// there) is removed first, and the temporary file is then created new, so
/// the bytes never go through a file or link that was already there.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder)?;
    }
    let temporary = temporary_path(path);
    match fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let replaced = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&temporary, path));
    if replaced.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    replaced
}

/// The contents of every copy kept aside in `folder` whose name starts with
/// `stem` (`settings.ignored`), in no particular order.
#[cfg(test)]
pub fn copies_kept_aside(folder: &Path, stem: &str) -> Vec<Vec<u8>> {
    fs::read_dir(folder)
        .expect("the folder can be listed")
        .map(|entry| entry.expect("an entry").path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(stem))
        })
        .map(|path| fs::read(path).expect("a copy can be read"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug_log::test_dir;

    #[test]
    fn a_write_replaces_the_whole_file_and_leaves_no_temporary_file() {
        let path = test_dir("data-folder-write")
            .join("nested")
            .join("file.json");
        write_atomically(&path, b"first, longer contents").expect("written");
        write_atomically(&path, b"second").expect("written again");
        assert_eq!(fs::read(&path).expect("the file is there"), b"second");
        assert!(!temporary_path(&path).exists());
    }

    #[test]
    fn a_failed_write_keeps_the_old_file_whole() {
        let path = test_dir("data-folder-failed").join("file.json");
        write_atomically(&path, b"old contents").expect("written");
        // A folder where the temporary file goes: the write fails before
        // anything replaces the file.
        fs::create_dir(temporary_path(&path)).expect("the folder is made");
        assert!(write_atomically(&path, b"new").is_err());
        assert_eq!(fs::read(&path).expect("still there"), b"old contents");
    }

    #[test]
    fn a_link_planted_at_the_temporary_path_is_not_written_through() {
        let folder = test_dir("data-folder-link");
        let path = folder.join("file.json");
        let victim = folder.join("victim.txt");
        fs::write(&victim, b"not yours").expect("written");
        // A hard link needs no privileges, unlike a symbolic one.
        fs::hard_link(&victim, temporary_path(&path)).expect("linked");
        write_atomically(&path, b"settings").expect("written");
        assert_eq!(fs::read(&victim).expect("still there"), b"not yours");
        assert_eq!(fs::read(&path).expect("the file is there"), b"settings");
        assert!(!temporary_path(&path).exists());
    }

    #[test]
    fn each_process_writes_through_its_own_temporary_file() {
        let path = Path::new("folder").join("file.json");
        let temporary = temporary_path(&path);
        let name = temporary.file_name().and_then(|name| name.to_str());
        let expected = format!("file.{}.tmp", std::process::id());
        assert_eq!(name, Some(expected.as_str()));
    }

    #[test]
    fn a_copy_kept_aside_never_replaces_an_older_one() {
        let folder = test_dir("data-folder-keep-aside");
        let path = folder.join("file.json");
        assert!(
            keep_aside(&path, "file.kept.json")
                .expect("no file")
                .is_none()
        );
        fs::write(&path, b"first contents").expect("written");
        let first = keep_aside(&path, "file.kept.json")
            .expect("kept")
            .expect("a copy");
        assert_eq!(fs::read(&first).unwrap(), b"first contents");
        assert_eq!(fs::read(&path).unwrap(), b"first contents", "left in place");

        // Other contents get a copy of their own; the first one stays.
        fs::write(&path, b"second").expect("written");
        let second = keep_aside(&path, "file.kept.json")
            .expect("kept")
            .expect("a copy");
        assert_ne!(first, second);
        assert_eq!(fs::read(&first).unwrap(), b"first contents");
        assert_eq!(fs::read(&second).unwrap(), b"second");
        // The same contents again are kept already: no third copy.
        let again = keep_aside(&path, "file.kept.json")
            .expect("kept")
            .expect("a copy");
        assert_eq!(again, second);
        let mut kept = copies_kept_aside(&folder, "file.kept");
        kept.sort();
        assert_eq!(kept, [b"first contents".to_vec(), b"second".to_vec()]);

        // An unreadable file is an error, and no copy is made or changed.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(keep_aside(&path, "file.kept.json").is_err());
        assert_eq!(copies_kept_aside(&folder, "file.kept").len(), 2);
        assert_eq!(fs::read(&first).unwrap(), b"first contents");
    }

    #[test]
    fn a_copy_is_named_after_the_file_and_the_time_and_never_takes_a_name_in_use() {
        use std::time::UNIX_EPOCH;
        assert_eq!(
            file_stamp(UNIX_EPOCH + Duration::from_millis(86_400_000 + 3_723_456)),
            "1970-01-02T010203Z"
        );
        let folder = test_dir("data-folder-copy-names");
        let path = folder.join("file.json");
        let first = keep_bytes_aside(&path, "file.kept.json", b"one").expect("kept");
        let name = first.file_name().and_then(|name| name.to_str()).unwrap();
        assert!(
            name.starts_with("file.kept-") && name.ends_with("Z.json"),
            "{name}"
        );
        // The same second: the next copy gets a number after the time.
        let second = keep_bytes_aside(&path, "file.kept.json", b"two").expect("kept");
        let third = keep_bytes_aside(&path, "file.kept.json", b"three").expect("kept");
        assert_eq!(fs::read(&first).unwrap(), b"one");
        assert_eq!(fs::read(&second).unwrap(), b"two");
        assert_eq!(fs::read(&third).unwrap(), b"three");
        assert!(!path.exists(), "the file itself isn't touched");
    }

    #[test]
    fn a_leftover_temporary_file_from_a_crash_does_not_block_the_write() {
        let path = test_dir("data-folder-leftover").join("file.json");
        fs::write(temporary_path(&path), b"half a fi").expect("written");
        write_atomically(&path, b"whole").expect("written");
        assert_eq!(fs::read(&path).expect("the file is there"), b"whole");
        assert!(!temporary_path(&path).exists());
    }
}
