//! Whether this process runs from an MSIX package, that is, whether Windows
//! gave it package identity. It is asked once per process, before the debug
//! log opens and before the settings, the startup entry or the updater are
//! touched, and only through `GetCurrentPackageFullName`: a package can be
//! installed outside `WindowsApps`, so the exe's path proves nothing.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const ERROR_SUCCESS: u32 = 0;
pub const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
pub const APPMODEL_ERROR_NO_PACKAGE: u32 = 15700;

/// How many times the full name is read after the first call sized it.
const NAME_READS: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Identity {
    /// The package's full name; empty when Windows confirmed identity but
    /// never handed the name over.
    Packaged(String),
    Unpackaged,
    /// Windows answered with another code, which counts as unpackaged.
    Unexpected(u32),
}

impl Identity {
    pub fn is_packaged(&self) -> bool {
        matches!(self, Identity::Packaged(_))
    }

    pub fn full_name(&self) -> Option<&str> {
        match self {
            Identity::Packaged(name) => Some(name),
            Identity::Unpackaged | Identity::Unexpected(_) => None,
        }
    }
}

/// What the first, zero-length call's return code says.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    Packaged,
    Unpackaged,
    Other(u32),
}

pub fn answer(code: u32) -> Answer {
    match code {
        ERROR_SUCCESS | ERROR_INSUFFICIENT_BUFFER => Answer::Packaged,
        APPMODEL_ERROR_NO_PACKAGE => Answer::Unpackaged,
        other => Answer::Other(other),
    }
}

/// `GetCurrentPackageFullName`, so tests can answer for Windows.
pub trait PackageQuery {
    /// Writes the name into `name` when it fits, sets `length` to the name's
    /// length in UTF-16 units with its terminating null, and returns the code.
    fn full_name(&self, length: &mut u32, name: Option<&mut [u16]>) -> u32;
}

pub fn detect(query: &dyn PackageQuery) -> Identity {
    let mut length = 0;
    match answer(query.full_name(&mut length, None)) {
        Answer::Packaged => {}
        Answer::Unpackaged => return Identity::Unpackaged,
        Answer::Other(code) => return Identity::Unexpected(code),
    }
    for _ in 0..NAME_READS {
        let mut buffer = vec![0u16; length as usize];
        match query.full_name(&mut length, Some(&mut buffer)) {
            ERROR_SUCCESS => {
                let end = buffer.iter().position(|&unit| unit == 0);
                let name = &buffer[..end.unwrap_or(buffer.len())];
                return Identity::Packaged(String::from_utf16_lossy(name));
            }
            ERROR_INSUFFICIENT_BUFFER => {}
            _ => break,
        }
    }
    Identity::Packaged(String::new())
}

/// This process's identity, asked of Windows on the first call only.
pub fn current() -> &'static Identity {
    static CURRENT: OnceLock<Identity> = OnceLock::new();
    CURRENT.get_or_init(|| detect(&SystemQuery))
}

struct SystemQuery;

#[cfg(windows)]
impl PackageQuery for SystemQuery {
    fn full_name(&self, length: &mut u32, name: Option<&mut [u16]>) -> u32 {
        use windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
        use windows::core::PWSTR;
        let name = name.map(|buffer| PWSTR(buffer.as_mut_ptr()));
        // SAFETY: `length` is never more than the buffer's size: zero
        // without one, and the buffer's length as `detect` made it.
        unsafe { GetCurrentPackageFullName(length, name) }.0
    }
}

#[cfg(not(windows))]
impl PackageQuery for SystemQuery {
    fn full_name(&self, _length: &mut u32, _name: Option<&mut [u16]>) -> u32 {
        APPMODEL_ERROR_NO_PACKAGE
    }
}

/// The package's local folder (`ApplicationData.LocalFolder`), where a
/// packaged run keeps its data.
#[cfg(windows)]
pub fn local_folder() -> Option<PathBuf> {
    use windows::Storage::ApplicationData;
    let folder = ApplicationData::Current().and_then(|data| data.LocalFolder());
    folder
        .and_then(|folder| folder.Path())
        .ok()
        .map(|path| PathBuf::from(path.to_os_string()))
}

/// The package's local cache folder (`ApplicationData.LocalCacheFolder`),
/// left out of backup and restore, where a packaged run's debug log goes.
#[cfg(windows)]
pub fn local_cache_folder() -> Option<PathBuf> {
    use windows::Storage::ApplicationData;
    let folder = ApplicationData::Current().and_then(|data| data.LocalCacheFolder());
    folder
        .and_then(|folder| folder.Path())
        .ok()
        .map(|path| PathBuf::from(path.to_os_string()))
}

#[cfg(not(windows))]
pub fn local_folder() -> Option<PathBuf> {
    None
}

#[cfg(not(windows))]
pub fn local_cache_folder() -> Option<PathBuf> {
    None
}

/// The debug log's folder: the package's local cache when packaged, so the
/// unpackaged copy's `%TEMP%` log is never opened through the package's
/// file-system view; the temporary folder otherwise. `None` turns the log off.
pub fn log_folder(
    identity: &Identity,
    temp: impl FnOnce() -> PathBuf,
    local_cache: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    if identity.is_packaged() {
        local_cache()
    } else {
        Some(temp())
    }
}

/// The log's first line. The folder is named relative to `%LOCALAPPDATA%`,
/// or by its last component when it lies elsewhere, so the account name
/// never reaches the log.
pub fn log_line(
    identity: &Identity,
    folder: Option<&Path>,
    local_app_data: Option<&Path>,
) -> String {
    match identity {
        Identity::Unpackaged => "package: none".to_owned(),
        Identity::Unexpected(code) => {
            format!("package: none (GetCurrentPackageFullName returned {code})")
        }
        Identity::Packaged(name) => {
            let shown = folder.map(|folder| shown_folder(folder, local_app_data));
            format!(
                "package: {name}; debug log folder: {}",
                shown.unwrap_or_default()
            )
        }
    }
}

fn shown_folder(folder: &Path, local_app_data: Option<&Path>) -> String {
    let relative = local_app_data.and_then(|base| folder.strip_prefix(base).ok());
    match relative {
        Some(relative) => relative.display().to_string(),
        None => folder
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// Answers each call with the next code, writing `name` when a buffer
    /// that fits comes with a success.
    struct Scripted {
        name: &'static str,
        codes: RefCell<Vec<u32>>,
        calls: RefCell<Vec<u32>>,
    }

    impl Scripted {
        fn new(name: &'static str, codes: &[u32]) -> Scripted {
            Scripted {
                name,
                codes: RefCell::new(codes.iter().rev().copied().collect()),
                calls: RefCell::default(),
            }
        }
    }

    impl PackageQuery for Scripted {
        fn full_name(&self, length: &mut u32, name: Option<&mut [u16]>) -> u32 {
            self.calls.borrow_mut().push(*length);
            let code = self.codes.borrow_mut().pop().expect("no more calls");
            let units: Vec<u16> = self.name.encode_utf16().chain([0]).collect();
            if code == ERROR_SUCCESS
                && let Some(buffer) = name
            {
                buffer[..units.len()].copy_from_slice(&units);
            }
            *length = u32::try_from(units.len()).unwrap();
            code
        }
    }

    const NAME: &str = "AudioSync_0.9.0.0_x64__abcdefghjkmnp";

    #[test]
    fn the_first_call_s_code_decides_packaged_or_not() {
        assert_eq!(answer(ERROR_INSUFFICIENT_BUFFER), Answer::Packaged);
        assert_eq!(answer(ERROR_SUCCESS), Answer::Packaged);
        assert_eq!(answer(APPMODEL_ERROR_NO_PACKAGE), Answer::Unpackaged);
        for code in [87, 5, 1] {
            assert_eq!(answer(code), Answer::Other(code));
        }
    }

    #[test]
    fn a_packaged_process_reads_its_name_after_a_zero_length_call() {
        let query = Scripted::new(NAME, &[ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS]);
        assert_eq!(detect(&query), Identity::Packaged(NAME.to_owned()));
        let calls = query.calls.borrow();
        assert_eq!(calls[0], 0, "the first call passes a zero-length buffer");
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn no_package_and_any_other_code_count_as_unpackaged() {
        let query = Scripted::new(NAME, &[APPMODEL_ERROR_NO_PACKAGE]);
        assert_eq!(detect(&query), Identity::Unpackaged);
        let query = Scripted::new(NAME, &[87]);
        let identity = detect(&query);
        assert_eq!(identity, Identity::Unexpected(87));
        assert!(!identity.is_packaged());
        assert_eq!(identity.full_name(), None);
    }

    #[test]
    fn a_name_that_grew_between_calls_is_read_again() {
        let query = Scripted::new(
            NAME,
            &[
                ERROR_INSUFFICIENT_BUFFER,
                ERROR_INSUFFICIENT_BUFFER,
                ERROR_SUCCESS,
            ],
        );
        assert_eq!(detect(&query), Identity::Packaged(NAME.to_owned()));
    }

    #[test]
    fn the_test_process_has_no_package_and_asks_once() {
        assert!(!current().is_packaged());
        assert!(std::ptr::eq(current(), current()));
    }

    #[test]
    fn a_packaged_log_goes_to_the_local_cache_and_never_to_temp() {
        let cache = PathBuf::from("cache");
        let packaged = Identity::Packaged(NAME.to_owned());
        let folder = log_folder(
            &packaged,
            || panic!("temp is not asked"),
            || Some(cache.clone()),
        );
        assert_eq!(folder, Some(cache));
        assert_eq!(
            log_folder(&packaged, || panic!("temp is not asked"), || None),
            None
        );
        for unpackaged in [Identity::Unpackaged, Identity::Unexpected(87)] {
            let folder = log_folder(&unpackaged, || PathBuf::from("temp"), || panic!("no cache"));
            assert_eq!(folder, Some(PathBuf::from("temp")));
        }
    }

    #[test]
    fn the_first_line_names_the_package_and_no_absolute_path() {
        let base = Path::new(r"D:\Profiles\someone\AppData\Local");
        let cache = base
            .join("Packages")
            .join("AudioSync_abcdefghjkmnp")
            .join("LocalCache");
        let packaged = Identity::Packaged(NAME.to_owned());
        let line = log_line(&packaged, Some(&cache), Some(base));
        assert!(line.contains(NAME), "{line}");
        assert!(line.contains("LocalCache"), "{line}");
        assert!(!line.contains("someone"), "{line}");
        assert!(!line.contains(r"D:\"), "{line}");
        let elsewhere = Path::new(r"D:\elsewhere\someone\LogHere");
        let line = log_line(&packaged, Some(elsewhere), Some(base));
        assert!(line.ends_with("LogHere"), "{line}");
        assert!(!line.contains("someone"), "{line}");
        assert_eq!(
            log_line(&Identity::Unpackaged, None, Some(base)),
            "package: none"
        );
        assert!(log_line(&Identity::Unexpected(87), None, None).contains("87"));
    }
}
