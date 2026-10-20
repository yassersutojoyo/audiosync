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
//! never saved over unseen: each store keeps it aside first
//! ([`keep_aside`]), and refuses to save while it can't be read.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The app's data folder under `%LOCALAPPDATA%`.
pub const FOLDER_NAME: &str = "AudioSync";

/// How long a read that failed waits before its second try.
pub const READ_RETRY_PAUSE: Duration = Duration::from_millis(500);

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

/// Copies the file at `path` to `name` in the same folder (replacing an
/// older copy there) and returns the copy's path; `None` when there is no
/// file at `path`. A file that can't be read is an error: it is then not
/// kept, so the caller must not replace it.
pub fn keep_aside(path: &Path, name: &str) -> io::Result<Option<PathBuf>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let kept = path.with_file_name(name);
    write_atomically(&kept, &bytes)?;
    Ok(Some(kept))
}

/// The app's data folder, `%LOCALAPPDATA%\AudioSync`, or `None` when Windows
/// names no such folder. The folder may not exist yet.
pub fn folder() -> Option<PathBuf> {
    let local = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
    local.is_absolute().then(|| local.join(FOLDER_NAME))
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
    fn a_file_kept_aside_is_copied_whole_and_an_unreadable_one_is_an_error() {
        let folder = test_dir("data-folder-keep-aside");
        let path = folder.join("file.json");
        assert!(
            keep_aside(&path, "file.kept.json")
                .expect("no file")
                .is_none()
        );
        fs::write(folder.join("file.kept.json"), b"an older copy").expect("written");
        fs::write(&path, b"contents").expect("written");
        let kept = keep_aside(&path, "file.kept.json").expect("kept");
        assert_eq!(kept, Some(folder.join("file.kept.json")));
        assert_eq!(
            fs::read(folder.join("file.kept.json")).unwrap(),
            b"contents"
        );
        assert_eq!(fs::read(&path).unwrap(), b"contents", "left in place");
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(keep_aside(&path, "file.kept.json").is_err());
        assert_eq!(
            fs::read(folder.join("file.kept.json")).unwrap(),
            b"contents"
        );
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
