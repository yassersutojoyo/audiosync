//! The app's data folder, `%LOCALAPPDATA%\AudioSync`, and how files in it
//! are written.
//!
//! The folder holds the paired Apple TVs' credentials
//! (`crate::audio::pairings`) and the user's settings (`crate::settings`).
//! Each file is replaced as a whole: written to a temporary file next to it,
//! flushed to disk, then renamed over it, so a crash or a power cut leaves
//! the old file or the new one, never half of one.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The app's data folder under `%LOCALAPPDATA%`.
pub const FOLDER_NAME: &str = "AudioSync";

/// The app's data folder, `%LOCALAPPDATA%\AudioSync`, or `None` when Windows
/// names no such folder. The folder may not exist yet.
pub fn folder() -> Option<PathBuf> {
    let local = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
    local.is_absolute().then(|| local.join(FOLDER_NAME))
}

/// The temporary file `path` is written through: the same name with the
/// extension `tmp`, in the same folder, so the rename never crosses volumes.
pub fn temporary_path(path: &Path) -> PathBuf {
    path.with_extension("tmp")
}

/// Replaces the file at `path` with `bytes`, creating its folder if needed.
/// The bytes go to a temporary file, flushed to disk before it is renamed
/// over `path`; a temporary file left by a failed write is removed.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder)?;
    }
    let temporary = temporary_path(path);
    let replaced = fs::File::create(&temporary)
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
    fn a_leftover_temporary_file_from_a_crash_does_not_block_the_write() {
        let path = test_dir("data-folder-leftover").join("file.json");
        fs::write(temporary_path(&path), b"half a fi").expect("written");
        write_atomically(&path, b"whole").expect("written");
        assert_eq!(fs::read(&path).expect("the file is there"), b"whole");
        assert!(!temporary_path(&path).exists());
    }
}
