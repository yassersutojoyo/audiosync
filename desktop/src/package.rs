//! Whether this exe runs from an MSIX package (a Microsoft Store install),
//! asked once per process before the settings are read or the debug log
//! opens, and the folders Windows gives the package.
//!
//! The answer comes from `GetCurrentPackageFullName` alone, never from the
//! exe's path or the environment: a package can be installed outside
//! `WindowsApps`.

use std::path::PathBuf;

/// `GetCurrentPackageFullName`'s `APPMODEL_ERROR_NO_PACKAGE`.
#[cfg(any(windows, test))]
const NO_PACKAGE: u32 = 15700;

/// What `GetCurrentPackageFullName` says for a zero-length buffer.
#[cfg(any(windows, test))]
#[derive(Debug, PartialEq, Eq)]
enum Identity {
    Packaged,
    Unpackaged,
    /// Any other answer, which counts as unpackaged and is logged.
    Unknown(u32),
}

#[cfg(any(windows, test))]
fn identity(status: u32) -> Identity {
    // ERROR_SUCCESS and ERROR_INSUFFICIENT_BUFFER: there is a name to read.
    match status {
        0 | 122 => Identity::Packaged,
        NO_PACKAGE => Identity::Unpackaged,
        other => Identity::Unknown(other),
    }
}

/// The package this process runs from.
// Only Windows runs an app from a package.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct Package {
    pub full_name: String,
    /// `ApplicationData.LocalFolder`, removed with the package.
    pub local_folder: Option<PathBuf>,
    /// `ApplicationData.LocalCacheFolder`, which backups leave out.
    pub local_cache: Option<PathBuf>,
}

/// The debug log's line about the package: its full name and the log's
/// folder relative to `local_app_data`, so no line carries the path of the
/// user's profile (S309); `unknown` is an answer that counted as unpackaged.
#[cfg(any(windows, test))]
pub fn log_line(
    package: Option<&Package>,
    unknown: Option<u32>,
    local_app_data: Option<&std::path::Path>,
) -> String {
    let Some(package) = package else {
        return match unknown {
            Some(code) => format!("package: none (GetCurrentPackageFullName answered {code})"),
            None => "package: none".to_owned(),
        };
    };
    let folder = package.local_cache.as_deref().map(|cache| {
        match local_app_data.and_then(|base| cache.strip_prefix(base).ok()) {
            Some(relative) => std::path::Path::new("%LOCALAPPDATA%").join(relative),
            None => cache.file_name().map(PathBuf::from).unwrap_or_default(),
        }
    });
    match folder {
        Some(folder) => format!(
            "package: {}; debug log folder: {}",
            package.full_name,
            folder.display()
        ),
        None => format!("package: {}; no debug log folder", package.full_name),
    }
}

/// [`log_line`] for this process.
#[cfg(windows)]
pub fn this_log_line() -> String {
    let local_app_data = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    match detected() {
        Detected::Packaged(package) => log_line(Some(package), None, local_app_data.as_deref()),
        Detected::Unpackaged => log_line(None, None, None),
        Detected::Unknown(code) => log_line(None, Some(*code), None),
    }
}

/// What this process learned about its package at its first question.
#[cfg(windows)]
pub enum Detected {
    Packaged(Package),
    Unpackaged,
    Unknown(u32),
}

#[cfg(windows)]
pub fn detected() -> &'static Detected {
    static DETECTED: std::sync::OnceLock<Detected> = std::sync::OnceLock::new();
    DETECTED.get_or_init(system::detect)
}

/// The package this exe runs from, or `None` for an exe started on its own.
pub fn current() -> Option<&'static Package> {
    #[cfg(windows)]
    {
        match detected() {
            Detected::Packaged(package) => Some(package),
            Detected::Unpackaged | Detected::Unknown(_) => None,
        }
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(windows)]
mod system {
    use std::path::PathBuf;

    use windows::Storage::{ApplicationData, StorageFolder};
    use windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
    use windows::core::PWSTR;

    use super::{Detected, Identity, Package};

    pub fn detect() -> Detected {
        let mut length = 0u32;
        // SAFETY: a length query with no buffer; `length` is valid for a write.
        let status = unsafe { GetCurrentPackageFullName(&raw mut length, None) };
        match super::identity(status.0) {
            Identity::Unpackaged => return Detected::Unpackaged,
            Identity::Unknown(code) => return Detected::Unknown(code),
            Identity::Packaged => {}
        }
        let mut name = vec![0u16; length as usize];
        // SAFETY: `name` holds the `length` UTF-16 units Windows asked for.
        let status =
            unsafe { GetCurrentPackageFullName(&raw mut length, Some(PWSTR(name.as_mut_ptr()))) };
        if status.0 != 0 {
            return Detected::Unknown(status.0);
        }
        // The length counts the terminating NUL.
        name.truncate((length as usize).saturating_sub(1));
        let data = ApplicationData::Current().ok();
        let path = |folder: windows::core::Result<StorageFolder>| {
            folder
                .and_then(|folder| folder.Path())
                .ok()
                .map(|path| PathBuf::from(path.to_os_string()))
        };
        Detected::Packaged(Package {
            full_name: String::from_utf16_lossy(&name),
            local_folder: data.as_ref().and_then(|data| path(data.LocalFolder())),
            local_cache: data.as_ref().and_then(|data| path(data.LocalCacheFolder())),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn only_a_name_to_read_means_packaged() {
        assert_eq!(identity(122), Identity::Packaged);
        assert_eq!(identity(0), Identity::Packaged);
        assert_eq!(identity(NO_PACKAGE), Identity::Unpackaged);
        // ERROR_INVALID_PARAMETER and anything else: unpackaged, and logged.
        assert_eq!(identity(87), Identity::Unknown(87));
    }

    // A packaged app sees only the user-wide environment when started from
    // Start; through the alias, a shell's AUDIOSYNC_DEBUG reaches it.
    #[test]
    fn the_package_answers_to_audiosync_exe_from_a_shell() {
        let manifest = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("msix/AppxManifest.xml"),
        )
        .expect("the package manifest is readable");
        assert!(manifest.contains(r#"Category="windows.appExecutionAlias""#));
        assert!(manifest.contains(r#"Alias="audiosync.exe""#));
    }

    #[test]
    fn the_log_names_the_package_and_its_log_folder_without_the_profile_path() {
        let local = Path::new("profile").join("AppData").join("Local");
        let cache = local
            .join("Packages")
            .join("AudioSync_test")
            .join("LocalCache");
        let package = Package {
            full_name: "AudioSync_0.8.0.0_x64__test".to_owned(),
            local_folder: None,
            local_cache: Some(cache),
        };
        let relative = Path::new("%LOCALAPPDATA%")
            .join("Packages")
            .join("AudioSync_test")
            .join("LocalCache");
        assert_eq!(
            log_line(Some(&package), None, Some(&local)),
            format!(
                "package: AudioSync_0.8.0.0_x64__test; debug log folder: {}",
                relative.display()
            )
        );
        let elsewhere = Package {
            local_cache: Some(Path::new("elsewhere").join("LocalCache")),
            ..package
        };
        assert_eq!(
            log_line(Some(&elsewhere), None, Some(&local)),
            "package: AudioSync_0.8.0.0_x64__test; debug log folder: LocalCache"
        );
        assert_eq!(
            log_line(Some(&elsewhere), None, None),
            "package: AudioSync_0.8.0.0_x64__test; debug log folder: LocalCache"
        );
        let no_cache = Package {
            local_cache: None,
            ..elsewhere
        };
        assert_eq!(
            log_line(Some(&no_cache), None, Some(&local)),
            "package: AudioSync_0.8.0.0_x64__test; no debug log folder"
        );
        assert_eq!(log_line(None, None, Some(&local)), "package: none");
        assert_eq!(
            log_line(None, Some(87), Some(&local)),
            "package: none (GetCurrentPackageFullName answered 87)"
        );
    }
}
