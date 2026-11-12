//! Swapping the running exe by renames inside the install folder (spec
//! S301), undone in reverse on a failure (S302).

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use super::archive::NOTICES_NAME;
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
    /// The `.new` was gone at step 4 with no Windows security error; another
    /// copy's step 1 deletes it (spec S302).
    Vanished,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    name: String,
    exe: bool,
    renamed_aside: bool,
    renamed_in: bool,
}

/// What a completed swap renamed, so a failed restart can undo exactly that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Swapped(Vec<Progress>);

impl Swapped {
    /// What each file's swap did as bits, which an elevated swap's exit
    /// code carries back (spec S460): the exe set aside and put in place,
    /// then the notices present, set aside and put in place.
    pub fn bits(&self) -> u32 {
        self.0.iter().fold(0, |bits, part| {
            let shift = if part.exe { 0 } else { 2 };
            let present = if part.exe { 0 } else { 1 << 4 };
            bits | present
                | u32::from(part.renamed_aside) << shift
                | u32::from(part.renamed_in) << (shift + 1)
        })
    }

    pub fn from_bits(exe_name: &str, bits: u32) -> Swapped {
        let part = |name: &str, exe: bool, shift: u32| Progress {
            name: name.to_owned(),
            exe,
            renamed_aside: bits >> shift & 1 == 1,
            renamed_in: bits >> (shift + 1) & 1 == 1,
        };
        let mut parts = vec![part(exe_name, true, 0)];
        if bits & 1 << 4 != 0 {
            parts.push(part(NOTICES_NAME, false, 2));
        }
        Swapped(parts)
    }
}

/// Installs `exe` (and `notices`) into `dir` in place of the running files,
/// the exe under `exe_name`.
pub fn swap(
    dir: &Path,
    exe_name: &str,
    exe: &[u8],
    notices: Option<&[u8]>,
    files: &dyn Files,
    log: &mut StepLog,
) -> Result<Swapped, SwapError> {
    let mut done: Vec<Progress> = Vec::new();
    let mut parts = vec![(exe_name, exe, true)];
    parts.extend(notices.map(|bytes| (NOTICES_NAME, bytes, false)));
    for (name, source, is_exe) in parts {
        let pair = Pair::new(dir, name);
        let mut progress = Progress {
            name: name.to_owned(),
            exe: is_exe,
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
    let is_exe = progress.exe;
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
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
            ) && is_exe =>
        {
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
    let staged = holds(files, &pair.new, source);
    logged(
        log,
        format!("4: check {name}.new against the download"),
        staged,
    )
    .map_err(|error| step_4_error("verify", error))?;
    let into_place = files.rename(&pair.new, &pair.live);
    logged(log, format!("4: rename {name}.new to {name}"), into_place)
        .map_err(|error| step_4_error("rename into place", error))?;
    progress.renamed_in = true;
    Ok(())
}

/// Whether `path` holds exactly `source`: another account that can write
/// the folder may have replaced the `.new` since step 1 wrote it.
fn holds(files: &dyn Files, path: &Path, source: &[u8]) -> io::Result<()> {
    let mut staged = Vec::with_capacity(source.len());
    files
        .open(path)?
        .take(source.len() as u64 + 1)
        .read_to_end(&mut staged)?;
    if staged == source {
        Ok(())
    } else {
        Err(io::Error::from(io::ErrorKind::InvalidData))
    }
}

fn step_4_error(step: &'static str, error: io::Error) -> SwapError {
    if is_blocked(&error) {
        SwapError::Blocked
    } else if error.kind() == io::ErrorKind::NotFound {
        SwapError::Vanished
    } else {
        SwapError::Failed {
            step,
            code: code(&error),
        }
    }
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
        let pair = Pair::new(dir, &progress.name);
        let result = remove_if_there(files, &pair.new);
        log.push((
            restore_line("remove", &pair.new),
            result.as_ref().err().map_or(0, code),
        ));
    }
    let mut renames = Vec::new();
    for progress in done.iter().rev() {
        let pair = Pair::new(dir, &progress.name);
        if progress.renamed_in && !progress.renamed_aside {
            let result = files.remove(&pair.live);
            log.push((
                restore_line("remove the new", &pair.live),
                result.as_ref().err().map_or(0, code),
            ));
        }
        if progress.renamed_aside {
            // One move over the new file: a refused move leaves the new exe
            // in place, never a folder without one.
            let result = files.replace(&pair.bak, &pair.live);
            log.push((
                restore_line("rename back", &pair.bak),
                result.as_ref().err().map_or(0, code),
            ));
            if result.is_err() {
                renames.push((
                    progress.exe,
                    Rename {
                        bak: file_name(&pair.bak),
                        live: file_name(&pair.live),
                    },
                ));
            }
        }
    }
    if renames.is_empty() {
        return Ok(());
    }
    renames.sort_by_key(|(exe, _)| !exe);
    Err(SwapError::RestoreFailed {
        renames: renames.into_iter().map(|(_, rename)| rename).collect(),
    })
}

fn restore_line(what: &str, path: &Path) -> String {
    format!("restore: {what} {}", file_name(path))
}

pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

/// Undoes a completed swap whose restart failed (spec S303): each `.bak`
/// this swap made, the running one included, moves back over its new file,
/// and a new notices file with no `.bak` goes.
pub fn undo_completed(
    dir: &Path,
    swapped: &Swapped,
    files: &dyn Files,
    log: &mut StepLog,
) -> Result<(), SwapError> {
    restore(dir, &swapped.0, files, log)
}

#[cfg(all(test, windows))]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::updater::archive::EXE_NAME;
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
        fn replace(&self, from: &Path, to: &Path) -> io::Result<()> {
            match (self.fail)("rename", to) {
                Some(error) => Err(error),
                None => RealFiles.replace(from, to),
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
        swap(&dir, EXE_NAME, exe, Some(notices), &RealFiles, &mut log).expect("the swap works");
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
        swap(&dir, EXE_NAME, exe, None, &RealFiles, &mut log).expect("the swap works");
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
            swap(&dir, EXE_NAME, exe, None, &files, &mut log),
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
        let result = swap(&dir, EXE_NAME, exe, Some(notices), &files, &mut log);
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
        let result = swap(&dir, EXE_NAME, exe, Some(notices), &files, &mut log);
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

    // covers: S302, S315
    #[test]
    fn a_new_another_copy_deleted_before_step_4_reports_another_copy() {
        let (dir, exe, _) = install("new-taken");
        let new = dir.join("audiosync.exe.new");
        // Another copy's step 1 deletes this one's .new before step 3.
        let files = failing(move |op, path| {
            if op == "rename" && file_name(path) == "audiosync.exe.bak" {
                std::fs::remove_file(&new).unwrap();
            }
            None
        });
        let mut log = StepLog::new();
        assert_eq!(
            swap(&dir, EXE_NAME, exe, None, &files, &mut log),
            Err(SwapError::Vanished)
        );
        assert_eq!(read(dir.join(EXE_NAME)), "old exe");
    }

    // covers: S301, S302
    #[test]
    fn a_new_rewritten_before_step_4_is_never_renamed_into_place() {
        for (case, planted) in [
            ("new-rewritten", &b"other exe"[..]),
            ("new-longer", b"new exe and more"),
            ("new-shorter", b"new"),
        ] {
            let (dir, exe, _) = install(case);
            let new = dir.join("audiosync.exe.new");
            // Another account with write access to the folder replaces the
            // staged file between the write and the rename into place.
            let files = failing(move |op, path| {
                if op == "rename" && file_name(path) == "audiosync.exe.bak" {
                    std::fs::remove_file(&new).unwrap();
                    std::fs::write(&new, planted).unwrap();
                }
                None
            });
            let mut log = StepLog::new();
            let result = swap(&dir, EXE_NAME, exe, None, &files, &mut log);
            assert!(
                matches!(result, Err(SwapError::Failed { step: "verify", .. })),
                "{case}: {result:?}"
            );
            assert_eq!(read(dir.join(EXE_NAME)), "old exe", "{case}");
            assert_eq!(names(&dir), [NOTICES_NAME, EXE_NAME], "{case}");
        }
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
            swap(&dir, EXE_NAME, exe, None, &files, &mut log),
            Err(SwapError::ReadOnly)
        );
        assert_eq!(names(&dir), [NOTICES_NAME, EXE_NAME]);

        // Linux: an AppImage on a read-only mount (EROFS).
        let (dir, exe, _) = install("read-only-mount");
        let files = failing(|op, path| {
            (op == "create" && file_name(path) == "audiosync.exe.new")
                .then(|| io::Error::from(io::ErrorKind::ReadOnlyFilesystem))
        });
        assert_eq!(
            swap(&dir, EXE_NAME, exe, None, &files, &mut StepLog::new()),
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
            swap(&dir, EXE_NAME, exe, None, &RealFiles, &mut log),
            Err(SwapError::InUse)
        );
        assert_eq!(read(dir.join(EXE_NAME)), "old exe");
        assert!(!dir.join("audiosync.exe.new").exists());
        assert!(!dir.join("audiosync.exe.bak").exists());
    }

    // covers: S302
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
            fn replace(&self, from: &Path, to: &Path) -> io::Result<()> {
                RealFiles.replace(from, to)
            }
        }
        let (dir, exe, _) = install("swap-disk-full-copy");
        let mut log = StepLog::new();
        assert_eq!(
            swap(&dir, EXE_NAME, exe, None, &FullDisk, &mut log),
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

    // covers: S302
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
        let result = swap(&dir, EXE_NAME, exe, Some(notices), &files, &mut log);
        assert_eq!(
            result,
            Err(SwapError::RestoreFailed {
                renames: vec![rename_of(EXE_NAME), rename_of(NOTICES_NAME)]
            })
        );
        assert_eq!(read(dir.join("audiosync.exe.bak")), "old exe");
        assert_eq!(read(dir.join("THIRD-PARTY-NOTICES.txt.bak")), "old notices");
        // A refused move back leaves the folder an exe to start.
        assert_eq!(read(dir.join(EXE_NAME)), "new exe");

        // Only the notices pair failing names only that pair.
        let (dir, exe, notices) = install("restore-lists-notices");
        let files = failing(|op, path| {
            (op == "rename" && file_name(path) == NOTICES_NAME)
                .then(|| io::Error::from_raw_os_error(5))
        });
        let result = swap(&dir, EXE_NAME, exe, Some(notices), &files, &mut log);
        assert_eq!(
            result,
            Err(SwapError::RestoreFailed {
                renames: vec![rename_of(NOTICES_NAME)]
            })
        );
        assert_eq!(read(dir.join(EXE_NAME)), "old exe");
    }

    // covers: S303
    #[test]
    fn a_failed_relaunch_restores_the_bak() {
        let (dir, exe, notices) = install("relaunch-failed");
        let mut log = StepLog::new();
        let swapped = swap(&dir, EXE_NAME, exe, Some(notices), &RealFiles, &mut log).unwrap();
        undo_completed(&dir, &swapped, &RealFiles, &mut log).expect("the undo works");
        assert_eq!(read(dir.join(EXE_NAME)), "old exe");
        assert_eq!(read(dir.join(NOTICES_NAME)), "old notices");
        assert_eq!(names(&dir), [NOTICES_NAME, EXE_NAME]);
    }
}
