//! The update of an install for all users (spec S460): the install folder
//! under Program Files refuses the user's own swap, so the running exe
//! starts itself elevated to verify and swap the downloaded zip.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

use ed25519_dalek::VerifyingKey;

use super::appcast::{Arch, Update};
use super::archive;
use super::files::{Files, RealFiles};
use super::flow::{self, Env, Installed};
use super::signature;
use super::swap::{self, Rename, StepLog, Swapped};
use super::version::Version;
use crate::debug_log::DebugLog;

/// The elevated copy's first argument; the zip, the version and the
/// signature follow. It installs into its own exe's folder.
pub const MARKER: &str = "--install-update-elevated";

/// What Windows answers when the user declines the consent prompt.
const DECLINED: i32 = 1223;

/// Starts this exe elevated and waits for its exit code.
pub trait Elevate: Send + Sync {
    fn machine_dirs(&self) -> Vec<PathBuf>;
    fn run(&self, args: &[OsString]) -> io::Result<u32>;
}

/// Whether a swap refused in `install_dir` goes through one elevation
/// prompt: only for a folder inside one of `machine_dirs`, so an install
/// for one user never prompts.
pub fn elevates(install_dir: &Path, machine_dirs: &[PathBuf]) -> bool {
    let folded = |path: &Path| {
        path.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    };
    let dir = folded(install_dir);
    machine_dirs
        .iter()
        .map(|machine| folded(machine))
        .any(|machine| {
            !machine.is_empty()
                && dir
                    .strip_prefix(&machine)
                    .is_some_and(|rest| rest.starts_with('\\'))
        })
}

/// Runs the elevated copy for the verified download `zip` and reads its
/// outcome; a declined prompt counts as Cancel.
pub fn install(env: &Env, update: &Update, zip: &Path) -> Installed {
    let base64 = |text: &str| {
        !text.is_empty()
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"+/=".contains(&byte))
    };
    // Anything else could carry quotes into the elevated command line.
    if !update.enclosure.signature.as_deref().is_some_and(base64) {
        return Installed::BadSignature;
    }
    let args = [
        OsStr::new(MARKER),
        zip.as_os_str(),
        OsStr::new(&update.version_text),
        OsStr::new(update.enclosure.signature.as_deref().unwrap_or("")),
    ]
    .map(OsStr::to_os_string);
    let ran = env.elevate.run(&args);
    env.log
        .log(|| format!("updater: the elevated swap ended ({ran:?})"));
    match ran {
        Ok(code) => outcome_of(code, &env.exe_name),
        Err(error) if error.raw_os_error() == Some(DECLINED) => Installed::Cancelled,
        Err(_) => Installed::ReadOnly,
    }
}

const DONE: u32 = 1;
const BAD_SIGNATURE: u32 = 2;
const BROKEN_DOWNLOAD: u32 = 3;
const DISK_FULL: u32 = 4;
const READ_ONLY: u32 = 5;
const IN_USE: u32 = 6;
const FAILED: u32 = 7;
const RESTORE_FAILED: u32 = 8;
const BLOCKED: u32 = 9;
const VANISHED: u32 = 10;

/// The elevated copy's exit code for `installed`: the outcome in the low
/// byte, what the swap renamed or couldn't rename back above it.
pub fn outcome_code(installed: &Installed) -> u32 {
    match installed {
        Installed::Done(swapped) => DONE | swapped.bits() << 8,
        Installed::BadSignature => BAD_SIGNATURE,
        Installed::BrokenDownload => BROKEN_DOWNLOAD,
        Installed::DiskFull => DISK_FULL,
        Installed::ReadOnly => READ_ONLY,
        Installed::InUse => IN_USE,
        Installed::RestoreFailed { renames } => {
            let exe = renames
                .iter()
                .any(|rename| rename.live != archive::NOTICES_NAME);
            let notices = renames
                .iter()
                .any(|rename| rename.live == archive::NOTICES_NAME);
            RESTORE_FAILED | u32::from(exe) << 8 | u32::from(notices) << 9
        }
        Installed::Blocked => BLOCKED,
        Installed::Vanished => VANISHED,
        Installed::Cancelled | Installed::DownloadFailed | Installed::Failed => FAILED,
    }
}

/// Reads [`outcome_code`]; any other code, a crash's included, is a failure.
pub fn outcome_of(code: u32, exe_name: &str) -> Installed {
    let rename = |live: &str| Rename {
        bak: format!("{live}.bak"),
        live: live.to_owned(),
    };
    if code >> 13 != 0 {
        return Installed::Failed;
    }
    match code & 0xff {
        DONE => Installed::Done(Swapped::from_bits(exe_name, code >> 8)),
        BAD_SIGNATURE => Installed::BadSignature,
        BROKEN_DOWNLOAD => Installed::BrokenDownload,
        DISK_FULL => Installed::DiskFull,
        READ_ONLY => Installed::ReadOnly,
        IN_USE => Installed::InUse,
        RESTORE_FAILED => Installed::RestoreFailed {
            renames: [(1 << 8, exe_name), (1 << 9, archive::NOTICES_NAME)]
                .into_iter()
                .filter(|(bit, _)| code & bit != 0)
                .map(|(_, live)| rename(live))
                .collect(),
        },
        BLOCKED => Installed::Blocked,
        VANISHED => Installed::Vanished,
        _ => Installed::Failed,
    }
}

/// Runs the elevated copy's work when this process is one, with the
/// release key and the real files, and answers its exit code.
pub fn from_command_line() -> Option<i32> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.first().map(OsString::as_os_str) != Some(OsStr::new(MARKER)) {
        return None;
    }
    // No debug log: the user's own %TEMP% may redirect an elevated write.
    let log = DebugLog::default();
    let key = signature::key(signature::RELEASE_KEY);
    let running = Version::parse(env!("CARGO_PKG_VERSION"));
    let exe = std::env::current_exe().ok();
    let install_dir = exe.as_deref().and_then(Path::parent);
    let code = match (key, running, install_dir) {
        (Some(key), Some(running), Some(install_dir)) => run_args(
            &args,
            install_dir,
            &key,
            &running,
            Arch::own(),
            &RealFiles,
            &log,
        )
        .unwrap_or(FAILED),
        _ => FAILED,
    };
    Some(code as i32)
}

/// The elevated copy's work: reads `args` after the exe's own name, and
/// answers the exit code, or nothing when this is no elevated copy.
pub fn run_args(
    args: &[OsString],
    install_dir: &Path,
    key: &VerifyingKey,
    running: &Version,
    arch: Arch,
    files: &dyn Files,
    log: &DebugLog,
) -> Option<u32> {
    let [marker, zip, version, signature] = args else {
        return None;
    };
    if marker != MARKER {
        return None;
    }
    let signature = signature.to_str().filter(|text| !text.is_empty());
    let installed = swap_zip(
        Path::new(zip),
        &version.to_string_lossy(),
        signature,
        install_dir,
        key,
        running,
        arch,
        files,
        log,
    );
    log.log(|| format!("updater: elevated copy: {installed:?}"));
    Some(outcome_code(&installed))
}

/// Verifies the zip again, since the user's own processes could have
/// changed it after the waiting copy did, then swaps it in.
#[allow(clippy::too_many_arguments)]
fn swap_zip(
    zip: &Path,
    version: &str,
    signature: Option<&str>,
    install_dir: &Path,
    key: &VerifyingKey,
    running: &Version,
    arch: Arch,
    files: &dyn Files,
    log: &DebugLog,
) -> Installed {
    let bytes = match std::fs::read(zip) {
        Ok(bytes) => bytes,
        Err(error) => {
            log.log(|| format!("updater: elevated copy can't read the download ({error})"));
            return Installed::Failed;
        }
    };
    if !signature::verifies(&bytes, signature, key) {
        return Installed::BadSignature;
    }
    let Ok(unpacked) = archive::unpack(&bytes, arch) else {
        return Installed::BrokenDownload;
    };
    match (
        archive::product_version(&unpacked.exe),
        Version::parse(version),
    ) {
        (Some(reported), Some(offered)) if reported == offered && reported > *running => {}
        _ => return Installed::BrokenDownload,
    }
    let mut steps = StepLog::new();
    let swapped = swap::swap(
        install_dir,
        archive::EXE_NAME,
        &unpacked.exe,
        unpacked.notices.as_deref(),
        files,
        &mut steps,
    );
    for (step, code) in &steps {
        log.log(|| format!("updater: elevated swap {step} (error {code})"));
    }
    flow::outcome(swapped)
}

/// The program folders of the machine, from the environment.
#[cfg(windows)]
fn program_folders() -> Vec<PathBuf> {
    ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
        .into_iter()
        .filter_map(std::env::var_os)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// One argument for a Windows command line, in quotes.
#[cfg(any(windows, test))]
fn quoted(arg: &OsStr) -> String {
    let text = arg.to_string_lossy();
    // A backslash right before the closing quote would escape it.
    let tail = text.len() - text.trim_end_matches('\\').len();
    format!("\"{text}{}\"", "\\".repeat(tail))
}

/// The consent prompt through ShellExecuteExW's `runas` verb.
#[cfg(windows)]
pub struct RunAs;

#[cfg(windows)]
impl Elevate for RunAs {
    fn machine_dirs(&self) -> Vec<PathBuf> {
        program_folders()
    }

    fn run(&self, args: &[OsString]) -> io::Result<u32> {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows::Win32::System::Com::{
            COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
        };
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, INFINITE, WaitForSingleObject,
        };
        use windows::Win32::UI::Shell::{
            SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
        };
        use windows::core::PCWSTR;

        let wide = |text: &OsStr| text.encode_wide().chain([0]).collect::<Vec<u16>>();
        let exe = wide(std::env::current_exe()?.as_os_str());
        let line = args
            .iter()
            .map(|arg| quoted(arg))
            .collect::<Vec<_>>()
            .join(" ");
        let parameters = wide(OsStr::new(&line));
        let verb = wide(OsStr::new("runas"));
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
            lpVerb: PCWSTR(verb.as_ptr()),
            lpFile: PCWSTR(exe.as_ptr()),
            lpParameters: PCWSTR(parameters.as_ptr()),
            ..Default::default()
        };
        // ShellExecuteExW can hand the verb to a shell extension, which needs COM.
        // SAFETY: balanced below on success; a thread already in COM keeps its mode.
        let com =
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        // SAFETY: every string outlives the call and is NUL-terminated.
        let executed = unsafe { ShellExecuteExW(&mut info) };
        if com.is_ok() {
            // SAFETY: pairs the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
        executed.map_err(|error| io::Error::from_raw_os_error(error.code().0 & 0xffff))?;
        let process = info.hProcess;
        if process.is_invalid() {
            return Err(io::Error::other("no process handle"));
        }
        // SAFETY: `process` is the handle SEE_MASK_NOCLOSEPROCESS returned.
        let waited = unsafe { WaitForSingleObject(process, INFINITE) };
        let mut code = 0u32;
        let read = unsafe { GetExitCodeProcess(process, &mut code) };
        // SAFETY: closed once, after its last use.
        let _ = unsafe { CloseHandle(process) };
        if waited != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        read.map_err(|error| io::Error::from_raw_os_error(error.code().0 & 0xffff))?;
        Ok(code)
    }
}

/// Linux has no install for all users.
#[cfg(not(windows))]
pub struct Never;

#[cfg(not(windows))]
impl Elevate for Never {
    fn machine_dirs(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    fn run(&self, _: &[OsString]) -> io::Result<u32> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// No program folders: a refused swap offers the release page.
    #[cfg(windows)]
    pub struct NoElevate;

    #[cfg(windows)]
    impl Elevate for NoElevate {
        fn machine_dirs(&self) -> Vec<PathBuf> {
            Vec::new()
        }

        fn run(&self, _: &[OsString]) -> io::Result<u32> {
            Err(io::Error::other("no elevation in this test"))
        }
    }

    fn machine() -> Vec<PathBuf> {
        vec![
            PathBuf::from(r"C:\Program Files"),
            PathBuf::from(r"C:\Program Files (x86)"),
        ]
    }

    // covers: S460
    #[test]
    fn an_install_under_program_files_elevates() {
        assert!(elevates(
            Path::new(r"C:\Program Files\AudioSync"),
            &machine()
        ));
        assert!(elevates(
            Path::new(r"c:\PROGRAM FILES\AudioSync"),
            &machine()
        ));
        assert!(elevates(
            Path::new(r"C:\Program Files (x86)\AudioSync"),
            &machine()
        ));
    }

    // covers: S460
    #[test]
    fn an_install_for_one_user_never_elevates() {
        assert!(!elevates(
            Path::new(r"C:\Users\someone\AppData\Local\Programs\AudioSync"),
            &machine()
        ));
        assert!(!elevates(
            Path::new(r"C:\Program Files Extra\AudioSync"),
            &machine()
        ));
        assert!(!elevates(Path::new(r"C:\Program Files\AudioSync"), &[]));
    }

    // covers: S460
    #[test]
    fn the_elevated_exit_code_carries_the_outcome_back() {
        let exe = archive::EXE_NAME;
        for bits in [0b00011, 0b11111, 0b10011, 0b00001] {
            let code = outcome_code(&Installed::Done(Swapped::from_bits(exe, bits)));
            assert_eq!(code >> 8, bits, "{bits:b}");
            assert_eq!(
                outcome_of(code, exe),
                Installed::Done(Swapped::from_bits(exe, bits))
            );
        }
        let rename = |live: &str| Rename {
            bak: format!("{live}.bak"),
            live: live.to_owned(),
        };
        for installed in [
            Installed::BadSignature,
            Installed::BrokenDownload,
            Installed::DiskFull,
            Installed::ReadOnly,
            Installed::InUse,
            Installed::Failed,
            Installed::Blocked,
            Installed::Vanished,
            Installed::RestoreFailed {
                renames: vec![rename(exe), rename(archive::NOTICES_NAME)],
            },
            Installed::RestoreFailed {
                renames: vec![rename(archive::NOTICES_NAME)],
            },
        ] {
            assert_eq!(outcome_of(outcome_code(&installed), exe), installed);
        }
        // A crash or a copy that never ran the swap.
        for code in [0, 101, 0xC000_0005] {
            assert_eq!(outcome_of(code, exe), Installed::Failed, "{code:x}");
        }
    }

    #[test]
    fn a_trailing_backslash_keeps_its_closing_quote() {
        assert_eq!(
            quoted(OsStr::new(r"C:\Program Files\AudioSync")),
            r#""C:\Program Files\AudioSync""#
        );
        assert_eq!(quoted(OsStr::new(r"C:\dir\")), r#""C:\dir\\""#);
    }

    /// Real files, except that no `.new` can be created, as under Program
    /// Files without elevation.
    #[cfg(windows)]
    struct Refusing;

    #[cfg(windows)]
    impl Files for Refusing {
        fn create(
            &self,
            path: &Path,
            exclusive: bool,
        ) -> io::Result<Box<dyn super::super::files::Sink>> {
            if path.extension() == Some(OsStr::new("new")) {
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }
            RealFiles.create(path, exclusive)
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

    /// The elevated copy run in this process; `declined` answers as a
    /// declined prompt does.
    #[cfg(windows)]
    struct InProcess {
        dirs: Vec<PathBuf>,
        install_dir: PathBuf,
        key: VerifyingKey,
        declined: bool,
    }

    #[cfg(windows)]
    impl Elevate for InProcess {
        fn machine_dirs(&self) -> Vec<PathBuf> {
            self.dirs.clone()
        }

        fn run(&self, args: &[OsString]) -> io::Result<u32> {
            if self.declined {
                return Err(io::Error::from_raw_os_error(DECLINED));
            }
            let running = Version::parse("0.8.0").unwrap();
            let log = DebugLog::default();
            run_args(
                args,
                &self.install_dir,
                &self.key,
                &running,
                Arch::X64,
                &RealFiles,
                &log,
            )
            .ok_or_else(|| io::Error::other("no elevated copy"))
        }
    }

    // covers: S460
    #[cfg(windows)]
    #[test]
    fn a_refused_swap_under_a_program_folder_installs_through_the_elevated_copy() {
        use crate::updater::archive::tests::fake_exe_reporting;
        use crate::updater::flow::tests::{fixture, update_of};
        use std::sync::atomic::AtomicBool;
        use std::sync::{Arc, Mutex};

        let install = |name: &str, elevate: &dyn Fn(&Env) -> Arc<dyn Elevate>| {
            let mut f = fixture(name, false, Arc::new(Refusing), DebugLog::default());
            f.env.elevate = elevate(&f.env);
            let update = update_of(&f.env);
            let installed = flow::install(
                &f.env,
                &update,
                &|_| {},
                &AtomicBool::new(false),
                &Mutex::new(false),
            );
            let exe = std::fs::read(f.env.install_dir.join(archive::EXE_NAME)).unwrap();
            (installed, exe)
        };
        let program_folder = |declined: bool| {
            move |env: &Env| -> Arc<dyn Elevate> {
                Arc::new(InProcess {
                    dirs: vec![env.install_dir.parent().unwrap().to_owned()],
                    install_dir: env.install_dir.clone(),
                    key: env.key,
                    declined,
                })
            }
        };

        let (installed, exe) = install("elevated", &program_folder(false));
        assert!(matches!(installed, Installed::Done(_)), "{installed:?}");
        assert_eq!(exe, fake_exe_reporting([0, 9, 0, 0], b"new exe"));

        let (installed, exe) = install("elevated-declined", &program_folder(true));
        assert_eq!(installed, Installed::Cancelled);
        assert_eq!(exe, b"old exe");

        let (installed, exe) = install("elevated-one-user", &|_| Arc::new(NoElevate));
        assert_eq!(installed, Installed::ReadOnly);
        assert_eq!(exe, b"old exe");
    }

    /// Fails the test if the consent prompt would show.
    #[cfg(windows)]
    struct NoPrompt;

    #[cfg(windows)]
    impl Elevate for NoPrompt {
        fn machine_dirs(&self) -> Vec<PathBuf> {
            Vec::new()
        }

        fn run(&self, _: &[OsString]) -> io::Result<u32> {
            panic!("the elevated copy started");
        }
    }

    // covers: S460
    #[cfg(windows)]
    #[test]
    fn a_signature_that_is_not_base64_never_reaches_the_prompt() {
        use crate::updater::flow::tests::{fixture, update_of};
        use std::sync::Arc;

        let mut f = fixture(
            "elevated-quote",
            false,
            Arc::new(RealFiles),
            DebugLog::default(),
        );
        f.env.elevate = Arc::new(NoPrompt);
        let mut update = update_of(&f.env);
        let zip = f.env.temp_root.join("update.zip");
        for planted in [r#"abc" "C:\x"#, "", "abc def"] {
            update.enclosure.signature = Some(planted.to_owned());
            assert_eq!(
                install(&f.env, &update, &zip),
                Installed::BadSignature,
                "{planted:?}"
            );
        }
    }

    // covers: S460
    #[test]
    fn the_elevated_copy_takes_no_install_folder_from_its_arguments() {
        let key = signature::test_key::new().verifying_key();
        let args = [MARKER, "update.zip", "0.9.0", "c2ln", r"C:\Windows"].map(OsString::from);
        let running = Version::parse("0.8.0").unwrap();
        let log = DebugLog::default();
        assert_eq!(
            run_args(
                &args,
                Path::new("install"),
                &key,
                &running,
                Arch::X64,
                &RealFiles,
                &log
            ),
            None
        );
    }
}
