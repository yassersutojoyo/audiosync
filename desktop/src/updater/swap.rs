//! Swapping the running exe by renames inside the install folder (spec
//! S301), undone in reverse on a failure (S302).

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::archive::{EXE_NAME, NOTICES_NAME};
use super::files::{Files, is_blocked, is_disk_full, remove_if_there};

#[derive(Debug, PartialEq, Eq)]
pub enum SwapError {
    /// The install folder can't be written (for example under Program
    /// Files); the notice offers the release page.
    ReadOnly,
    /// A file is held by another copy of AudioSync.
    InUse,
    DiskFull,
    /// Any other failure, with the step and the OS error code.
    Failed {
        step: &'static str,
        code: i32,
    },
    /// The restore failed too: each file stays under its `.bak` name until
    /// the user renames it back, the exe first.
    RestoreFailed {
        renames: Vec<Rename>,
    },
    /// Windows security removed or blocked a file (spec S315).
    Blocked,
}

/// A rename the user has to make by hand: `bak` back to `live`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rename {
    pub bak: String,
    pub live: String,
}

/// One logged step: what it did and its OS error code, 0 for success.
pub type StepLog = Vec<(String, i32)>;

fn code(error: &io::Error) -> i32 {
    error.raw_os_error().unwrap_or(-1)
}

struct Pair {
    live: PathBuf,
    new: PathBuf,
    bak: PathBuf,
}

impl Pair {
    fn new(dir: &Path, name: &str) -> Pair {
        Pair {
            live: dir.join(name),
            new: dir.join(format!("{name}.new")),
            bak: dir.join(format!("{name}.bak")),
        }
    }
}

/// How far a pair got, for the restore.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    name: &'static str,
    renamed_aside: bool,
    renamed_in: bool,
}

/// What a completed swap renamed, so a failed restart can undo exactly that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Swapped(Vec<Progress>);

/// Installs `exe` (and `notices`) into `dir` in place of the running files.
pub fn swap(
    dir: &Path,
    exe: &[u8],
    notices: Option<&[u8]>,
    files: &dyn Files,
    log: &mut StepLog,
) -> Result<Swapped, SwapError> {
    let mut done: Vec<Progress> = Vec::new();
    let mut parts = vec![(EXE_NAME, exe)];
    parts.extend(notices.map(|bytes| (NOTICES_NAME, bytes)));
    for (name, source) in parts {
        let pair = Pair::new(dir, name);
        let mut progress = Progress {
            name,
            renamed_aside: false,
            renamed_in: false,
        };
        let result = swap_one(&pair, name, source, files, log, &mut progress);
        done.push(progress);
        if let Err(error) = result {
            restore(dir, &done, files, log)?;
            return Err(error);
        }
    }
    Ok(Swapped(done))
}

fn logged<T>(log: &mut StepLog, what: String, result: io::Result<T>) -> io::Result<T> {
    log.push((what, result.as_ref().err().map_or(0, code)));
    result
}

fn swap_one(
    pair: &Pair,
    name: &str,
    source: &[u8],
    files: &dyn Files,
    log: &mut StepLog,
    progress: &mut Progress,
) -> Result<(), SwapError> {
    let is_exe = name == EXE_NAME;
    let leftover = remove_if_there(files, &pair.new);
    logged(log, format!("1: remove a leftover {name}.new"), leftover).map_err(in_use)?;
    let created = logged(
        log,
        format!("1: create {name}.new"),
        files.create(&pair.new, true),
    );
    let mut out = match created {
        Ok(out) => out,
        Err(error) if is_blocked(&error) => return Err(SwapError::Blocked),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied && is_exe => {
            return Err(SwapError::ReadOnly);
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Err(SwapError::InUse),
        Err(error) if is_disk_full(&error) => return Err(SwapError::DiskFull),
        Err(error) => {
            return Err(SwapError::Failed {
                step: "create",
                code: code(&error),
            });
        }
    };
    let copied = out
        .write_all(source)
        .and_then(|()| out.flush())
        .and_then(|()| out.sync());
    drop(out);
    if let Err(error) = logged(log, format!("1: copy into {name}.new"), copied) {
        return Err(if is_blocked(&error) {
            SwapError::Blocked
        } else if is_disk_full(&error) {
            SwapError::DiskFull
        } else {
            SwapError::Failed {
                step: "copy",
                code: code(&error),
            }
        });
    }
    let earlier = remove_if_there(files, &pair.bak);
    logged(log, format!("2: remove an earlier {name}.bak"), earlier).map_err(in_use)?;
    match files.rename(&pair.live, &pair.bak) {
        Ok(()) => {
            log.push((format!("3: rename {name} to {name}.bak"), 0));
            progress.renamed_aside = true;
        }
        // A notices file the folder never had leaves nothing to set aside.
        Err(error) if !is_exe && error.kind() == io::ErrorKind::NotFound => {
            log.push((format!("3: no {name} to set aside"), code(&error)));
        }
        Err(error) => {
            log.push((format!("3: rename {name} to {name}.bak"), code(&error)));
            return Err(in_use(error));
        }
    }
    let into_place = files.rename(&pair.new, &pair.live);
    logged(log, format!("4: rename {name}.new to {name}"), into_place).map_err(|error| {
        // A `.new` gone since step 1 was removed by Windows security.
        if is_blocked(&error) || error.kind() == io::ErrorKind::NotFound {
            SwapError::Blocked
        } else {
            SwapError::Failed {
                step: "rename into place",
                code: code(&error),
            }
        }
    })?;
    progress.renamed_in = true;
    Ok(())
}

/// A refused step 1, 2 or 3: another copy holds the file, unless Windows
/// security blocked it.
fn in_use(error: io::Error) -> SwapError {
    if is_blocked(&error) {
        SwapError::Blocked
    } else {
        SwapError::InUse
    }
}

fn restore(
    dir: &Path,
    done: &[Progress],
    files: &dyn Files,
    log: &mut StepLog,
) -> Result<(), SwapError> {
    for progress in done.iter().rev() {
        let pair = Pair::new(dir, progress.name);
        let result = remove_if_there(files, &pair.new);
        log.push((
            restore_line("remove", &pair.new),
            result.as_ref().err().map_or(0, code),
        ));
    }
    let mut renames = Vec::new();
    for progress in done.iter().rev() {
        let pair = Pair::new(dir, progress.name);
        if progress.renamed_in {
            let result = files.remove(&pair.live);
            log.push((
                restore_line("remove the new", &pair.live),
                result.as_ref().err().map_or(0, code),
            ));
        }
        if progress.renamed_aside {
            let result = files.rename(&pair.bak, &pair.live);
            log.push((
                restore_line("rename back", &pair.bak),
                result.as_ref().err().map_or(0, code),
            ));
            if result.is_err() {
                renames.push(Rename {
                    bak: file_name(&pair.bak),
                    live: file_name(&pair.live),
                });
            }
        }
    }
    if renames.is_empty() {
        return Ok(());
    }
    renames.sort_by_key(|rename| rename.live != EXE_NAME);
    Err(SwapError::RestoreFailed { renames })
}

fn restore_line(what: &str, path: &Path) -> String {
    format!("restore: {what} {}", file_name(path))
}

pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

/// Undoes a completed swap whose restart failed (spec S303): the new
/// files go, then each `.bak` this swap made, the running one included, is
/// renamed back.
pub fn undo_completed(
    dir: &Path,
    swapped: &Swapped,
    files: &dyn Files,
    log: &mut StepLog,
) -> Result<(), SwapError> {
    restore(dir, &swapped.0, files, log)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::updater::files::{RealFiles, Sink};
    use crate::updater::tests::test_dir;

    type Fail = Box<dyn Fn(&str, &Path) -> Option<io::Error> + Send + Sync>;

    /// Real files, except where `fail` answers an error for the operation
    /// and its path.
    pub struct FailingFiles {
        pub fail: Fail,
        pub calls: Mutex<Vec<String>>,
    }

    impl Files for FailingFiles {
        fn create(&self, path: &Path, exclusive: bool) -> io::Result<Box<dyn Sink>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("create {}", file_name(path)));
            match (self.fail)("create", path) {
                Some(error) => Err(error),
                None => RealFiles.create(path, exclusive),
            }
        }
        fn remove(&self, path: &Path) -> io::Result<()> {
            match (self.fail)("remove", path) {
                Some(error) => Err(error),
                None => RealFiles.remove(path),
            }
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            match (self.fail)("rename", to) {
                Some(error) => Err(error),
                None => RealFiles.rename(from, to),
            }
        }
    }

    fn failing(
        fail: impl Fn(&str, &Path) -> Option<io::Error> + Send + Sync + 'static,
    ) -> FailingFiles {
        FailingFiles {
            fail: Box::new(fail),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// An install folder with the old exe and notices, and the new exe and
    /// notices.
    fn install(name: &str) -> (PathBuf, &'static [u8], &'static [u8]) {
        let install = test_dir(name).join("install");
        std::fs::create_dir_all(&install).unwrap();
        std::fs::write(install.join(EXE_NAME), b"old exe").unwrap();
        std::fs::write(install.join(NOTICES_NAME), b"old notices").unwrap();
        (install, b"new exe", b"new notices")
    }

    fn read(path: PathBuf) -> String {
        std::fs::read_to_string(path).unwrap_or_else(|_| "<none>".to_owned())
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    // covers: S301
    #[test]
    fn a_swap_keeps_the_old_exe_as_bak() {
        let (dir, exe, notices) = install("swap-ok");
        std::fs::write(dir.join("audiosync.exe.bak"), b"older").unwrap();
        let mut log = StepLog::new();
        swap(&dir, exe, Some(notices), &RealFiles, &mut log).expect("the swap works");
        assert_eq!(read(dir.join(EXE_NAME)), "new exe");
        assert_eq!(read(dir.join("audiosync.exe.bak")), "old exe");
        assert_eq!(read(dir.join(NOTICES_NAME)), "new notices");
        assert_eq!(read(dir.join("THIRD-PARTY-NOTICES.txt.bak")), "old notices");
        assert!(log.iter().all(|(_, code)| *code == 0), "{log:?}");
    }

    // covers: S301, S302
    #[test]
    fn a_leftover_new_is_deleted_before_the_exclusive_create() {
        let (dir, exe, _) = install("leftover-new");
        std::fs::write(dir.join("audiosync.exe.new"), b"stale").unwrap();
        let mut log = StepLog::new();
        swap(&dir, exe, None, &RealFiles, &mut log).expect("the swap works");
        assert_eq!(read(dir.join(EXE_NAME)), "new exe");
        assert!(!dir.join("audiosync.exe.new").exists());

        // A .new that appears again between the delete and the create is
        // another copy at work.
        let (dir, exe, _) = install("leftover-new-race");
        let files = failing(|op, path| {
            (op == "create" && file_name(path) == "audiosync.exe.new")
                .then(|| io::Error::from(io::ErrorKind::AlreadyExists))
        });
        assert_eq!(
            swap(&dir, exe, None, &files, &mut log),
            Err(SwapError::InUse)
        );
        assert_eq!(read(dir.join(EXE_NAME)), "old exe");
    }

    // covers: S301, S302
    #[test]
    fn a_failed_second_rename_restores_the_bak() {
        let (dir, exe, notices) = install("second-rename");
        let files = failing(|op, path| {
            (op == "rename" && file_name(path) == EXE_NAME).then(|| io::Error::from_raw_os_error(5))
        });
        let mut log = StepLog::new();
        let result = swap(&dir, exe, Some(notices), &files, &mut log);
        // The rename back targets audiosync.exe too, which this double
        // refuses, so the restore names the .bak.
        assert_eq!(
            result,
            Err(SwapError::RestoreFailed {
                renames: vec![rename_of(EXE_NAME)]
            })
        );
        assert_eq!(read(dir.join("audiosync.exe.bak")), "old exe");

        // With the rename back allowed, the folder is as before.
        let (dir, exe, notices) = install("second-rename-restored");
        let refused = std::sync::atomic::AtomicBool::new(false);
        let files = failing(move |op, path| {
            (op == "rename"
                && file_name(path) == NOTICES_NAME
                && !refused.swap(true, std::sync::atomic::Ordering::SeqCst))
            .then(|| io::Error::from_raw_os_error(5))
        });
        let result = swap(&dir, exe, Some(notices), &files, &mut log);
        assert_eq!(
            result,
            Err(SwapError::Failed {
                step: "rename into place",
                code: 5
            })
        );
        assert_eq!(read(dir.join(EXE_NAME)), "old exe");
        assert_eq!(read(dir.join(NOTICES_NAME)), "old notices");
        assert_eq!(names(&dir), [NOTICES_NAME, EXE_NAME]);
    }

    // covers: S302
    #[test]
    fn a_read_only_install_folder_offers_the_release_page() {
        let (dir, exe, _) = install("read-only");
        let files = failing(|op, path| {
            (op == "create" && file_name(path) == "audiosync.exe.new")
                .then(|| io::Error::from(io::ErrorKind::PermissionDenied))
        });
        let mut log = StepLog::new();
        assert_eq!(
            swap(&dir, exe, None, &files, &mut log),
            Err(SwapError::ReadOnly)
        );
        assert_eq!(names(&dir), [NOTICES_NAME, EXE_NAME]);
    }

    // covers: S302
    #[test]
    fn a_locked_exe_reports_another_copy_and_changes_nothing() {
        use std::os::windows::fs::OpenOptionsExt;
        let (dir, exe, _) = install("locked");
        // Shared for reading only, as a running image is: no delete share,
        // so the rename aside is refused.
        let _held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(dir.join(EXE_NAME))
            .unwrap();
        let mut log = StepLog::new();
        assert_eq!(
            swap(&dir, exe, None, &RealFiles, &mut log),
            Err(SwapError::InUse)
        );
        assert_eq!(read(dir.join(EXE_NAME)), "old exe");
        assert!(!dir.join("audiosync.exe.new").exists());
        assert!(!dir.join("audiosync.exe.bak").exists());
    }

    #[test]
    fn a_disk_full_copy_deletes_every_partial_file() {
        struct Full;
        impl Write for Full {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from_raw_os_error(112))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl Sink for Full {
            fn sync(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        struct FullDisk;
        impl Files for FullDisk {
            fn create(&self, path: &Path, exclusive: bool) -> io::Result<Box<dyn Sink>> {
                RealFiles.create(path, exclusive)?;
                Ok(Box::new(Full))
            }
            fn remove(&self, path: &Path) -> io::Result<()> {
                RealFiles.remove(path)
            }
            fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
                RealFiles.rename(from, to)
            }
        }
        let (dir, exe, _) = install("swap-disk-full-copy");
        let mut log = StepLog::new();
        assert_eq!(
            swap(&dir, exe, None, &FullDisk, &mut log),
            Err(SwapError::DiskFull)
        );
        assert_eq!(names(&dir), [NOTICES_NAME, EXE_NAME]);
    }

    // covers: S301
    #[test]
    fn a_running_exe_can_be_renamed_aside_and_back() {
        // The copy goes even when the test fails, after its process, which
        // locks the image, has ended.
        struct RunningCopy {
            dir: PathBuf,
            child: Option<std::process::Child>,
        }
        impl Drop for RunningCopy {
            fn drop(&mut self) {
                if let Some(mut child) = self.child.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }
        // Not a copy of this test binary: in the shared target folder other
        // worktrees' cargo deletes and relinks it while the suite runs. Sort
        // runs until its input ends, so it is alive through both renames.
        let mut copy = RunningCopy {
            dir: test_dir("running-rename"),
            child: None,
        };
        let running = copy.dir.join(EXE_NAME);
        let windows = PathBuf::from(std::env::var_os("SystemRoot").expect("Windows' folder"));
        std::fs::copy(windows.join(r"System32\sort.exe"), &running).unwrap();
        let child = std::process::Command::new(&running)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("the copy starts");
        let child = copy.child.insert(child);
        let aside = running.with_extension("exe.renamed-aside");
        let there = RealFiles.rename(&running, &aside);
        let back = there
            .as_ref()
            .ok()
            .map(|_| RealFiles.rename(&aside, &running));
        drop(child.stdin.take());
        let exit = child.wait().expect("the copy exits");
        there.expect("a running image renames aside");
        back.unwrap().expect("and back");
        assert!(exit.success(), "the copy ran while renamed: {exit}");
        assert!(running.exists());
    }

    fn rename_of(live: &str) -> Rename {
        Rename {
            bak: format!("{live}.bak"),
            live: live.to_owned(),
        }
    }

    #[test]
    fn a_failed_restore_lists_every_bak_with_its_name_the_exe_first() {
        // The double sees a rename's target. The exe's first rename into
        // audiosync.exe (step 4) works and every later one (the rename back)
        // is refused; every rename into the notices name is refused, so the
        // notices step 4 fails and its restore fails too. The restore runs
        // newest first, yet the exe leads the list: the user needs it back.
        let (dir, exe, notices) = install("restore-lists-both");
        let exe_renames = std::sync::atomic::AtomicUsize::new(0);
        let files = failing(move |op, path| {
            let refused = op == "rename"
                && match file_name(path).as_str() {
                    EXE_NAME => exe_renames.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0,
                    NOTICES_NAME => true,
                    _ => false,
                };
            refused.then(|| io::Error::from_raw_os_error(5))
        });
        let mut log = StepLog::new();
        let result = swap(&dir, exe, Some(notices), &files, &mut log);
        assert_eq!(
            result,
            Err(SwapError::RestoreFailed {
                renames: vec![rename_of(EXE_NAME), rename_of(NOTICES_NAME)]
            })
        );
        assert_eq!(read(dir.join("audiosync.exe.bak")), "old exe");
        assert_eq!(read(dir.join("THIRD-PARTY-NOTICES.txt.bak")), "old notices");

        // Only the notices pair failing names only that pair.
        let (dir, exe, notices) = install("restore-lists-notices");
        let files = failing(|op, path| {
            (op == "rename" && file_name(path) == NOTICES_NAME)
                .then(|| io::Error::from_raw_os_error(5))
        });
        let result = swap(&dir, exe, Some(notices), &files, &mut log);
        assert_eq!(
            result,
            Err(SwapError::RestoreFailed {
                renames: vec![rename_of(NOTICES_NAME)]
            })
        );
        assert_eq!(read(dir.join(EXE_NAME)), "old exe");
    }

    #[test]
    fn a_failed_relaunch_restores_the_bak() {
        let (dir, exe, notices) = install("relaunch-failed");
        let mut log = StepLog::new();
        let swapped = swap(&dir, exe, Some(notices), &RealFiles, &mut log).unwrap();
        undo_completed(&dir, &swapped, &RealFiles, &mut log).expect("the undo works");
        assert_eq!(read(dir.join(EXE_NAME)), "old exe");
        assert_eq!(read(dir.join(NOTICES_NAME)), "old notices");
        assert_eq!(names(&dir), [NOTICES_NAME, EXE_NAME]);
    }
}
