//! Linux, by the freedesktop.org specifications: the XDG base directories
//! for the data folder and the debug log, the single-instance lock and
//! socket in the runtime directory ([`instance`]), the autostart entry
//! ([`autostart`]), and the pairing files kept as owner-only files instead
//! of being encrypted.
//!
//! Folders this makes get mode 0700, as the base directory specification
//! asks; std alone reads the variables and the modes.

pub mod autostart;
pub mod instance;

use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::audio::pairings::{Seal, Sealer};

pub use instance::{Instance, claim_instance};

/// The id the Mac app's bundle and the Windows app's named objects use; on
/// Linux it names the autostart entry and the icon.
pub const APP_ID: &str = "io.github.yassersutojoyo.audiosync";

/// The folder under each base directory.
pub const FOLDER_NAME: &str = "audiosync";

/// The debug log's file name in the state folder.
pub const LOG_FILE_NAME: &str = "debug.log";

/// `value` when it is an absolute path, else `fallback` (relative to the
/// home folder) under `home`; a relative value is invalid by the
/// specification and ignored.
fn base_dir(value: Option<OsString>, home: Option<&Path>, fallback: &str) -> Option<PathBuf> {
    if let Some(value) = value.map(PathBuf::from)
        && value.is_absolute()
    {
        return Some(value);
    }
    home.filter(|home| home.is_absolute())
        .map(|home| home.join(fallback))
}

fn home() -> Option<PathBuf> {
    std::env::home_dir().filter(|home| !home.as_os_str().is_empty())
}

fn env_base(var: &str, fallback: &str) -> Option<PathBuf> {
    base_dir(std::env::var_os(var), home().as_deref(), fallback)
}

/// `$XDG_DATA_HOME`, else `~/.local/share`: the data folder's base.
#[cfg_attr(
    all(not(os_backend), not(test)),
    expect(dead_code, reason = "without an audio backend nothing is stored")
)]
pub fn data_base() -> Option<PathBuf> {
    env_base("XDG_DATA_HOME", ".local/share")
}

/// `$XDG_STATE_HOME`, else `~/.local/state`: where the debug log goes, so it
/// is still there after the user signs out.
pub fn state_base() -> Option<PathBuf> {
    env_base("XDG_STATE_HOME", ".local/state")
}

/// `$XDG_CONFIG_HOME`, else `~/.config`: the autostart folder's base.
pub fn config_base() -> Option<PathBuf> {
    env_base("XDG_CONFIG_HOME", ".config")
}

/// The user this process runs as: the owner of `/proc/self`, else of the
/// home folder. std has no call for it.
pub fn user_id() -> Option<u32> {
    let metadata = fs::metadata("/proc/self")
        .ok()
        .or_else(|| fs::metadata(home()?).ok())?;
    Some(metadata.uid())
}

/// Creates `folder` and the missing folders above it with mode 0700; an
/// existing one is left as it is.
pub fn create_private_dir_all(folder: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(folder)
}

/// `folder`, created with mode 0700 when missing, checked to be a real
/// folder that `user` owns and no one else may enter; one with group or
/// other bits gets 0700.
pub fn owned_private_dir(folder: &Path, user: u32) -> io::Result<()> {
    if let Err(error) = DirBuilder::new().mode(0o700).create(folder)
        && error.kind() != io::ErrorKind::AlreadyExists
    {
        return Err(error);
    }
    let metadata = fs::symlink_metadata(folder)?;
    if !metadata.is_dir() || metadata.uid() != user {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} isn't a folder of this user's", folder.display()),
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        fs::set_permissions(folder, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// `$XDG_RUNTIME_DIR` when it is absolute, a folder `user` owns and mode
/// 0700, as the specification promises; `None` otherwise.
pub fn runtime_base(value: Option<OsString>, user: u32) -> Option<PathBuf> {
    let dir = PathBuf::from(value?);
    let metadata = fs::symlink_metadata(&dir).ok()?;
    (dir.is_absolute()
        && metadata.is_dir()
        && metadata.uid() == user
        && metadata.mode() & 0o777 == 0o700)
        .then_some(dir)
}

/// Gives a file or folder that other accounts may read or enter
/// `private_mode`; returns whether it had to. `PermissionDenied` means
/// another account owns it. A path that isn't there is left alone.
#[cfg_attr(
    not(os_backend),
    allow(
        dead_code,
        reason = "the pairing store calls it, which may be unused without a backend"
    )
)]
pub fn tighten(path: &Path, private_mode: u32) -> io::Result<bool> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if metadata.mode() & 0o077 == 0 {
        return Ok(false);
    }
    fs::set_permissions(path, fs::Permissions::from_mode(private_mode))?;
    Ok(true)
}

/// The debug log in `state_base`'s `audiosync` folder, created with mode
/// 0700; `Err` says why it can't be used.
pub fn debug_log_file(state_base: Option<PathBuf>) -> Result<PathBuf, String> {
    let folder = state_base
        .ok_or_else(|| "neither XDG_STATE_HOME nor a home folder is set".to_owned())?
        .join(FOLDER_NAME);
    create_private_dir_all(&folder).map_err(|error| format!("{}: {error}", folder.display()))?;
    Ok(folder.join(LOG_FILE_NAME))
}

/// No encryption: Linux keeps the pairing files private by their mode
/// (0600, in a 0700 folder) and checks it at launch, as the Mac does.
pub fn sealer() -> Sealer {
    Sealer::new(&OwnerOnly)
}

struct OwnerOnly;

impl Seal for OwnerOnly {
    fn seal(&self, plain: &[u8]) -> io::Result<Vec<u8>> {
        Ok(plain.to_vec())
    }

    fn open(&self, sealed: &[u8]) -> io::Result<zeroize::Zeroizing<Vec<u8>>> {
        Ok(zeroize::Zeroizing::new(sealed.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug_log::test_dir;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().mode() & 0o7777
    }

    #[test]
    fn a_variable_counts_only_when_it_is_absolute() {
        let home = Path::new("/home/me");
        assert_eq!(
            base_dir(Some("/data".into()), Some(home), ".local/share"),
            Some(PathBuf::from("/data"))
        );
        for ignored in [None, Some(OsString::new()), Some("relative/data".into())] {
            assert_eq!(
                base_dir(ignored, Some(home), ".local/share"),
                Some(PathBuf::from("/home/me/.local/share"))
            );
        }
        assert_eq!(base_dir(None, None, ".config"), None);
        assert_eq!(base_dir(None, Some(Path::new("home")), ".config"), None);
    }

    #[test]
    fn new_folders_are_owner_only_and_existing_ones_keep_their_mode() {
        let dir = test_dir("xdg-private-dirs");
        let deep = dir.join("a").join("b");
        create_private_dir_all(&deep).unwrap();
        assert_eq!(mode(&dir.join("a")), 0o700);
        assert_eq!(mode(&deep), 0o700);

        let open = dir.join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir_all(&open).unwrap();
        assert_eq!(mode(&open), 0o755);
    }

    #[test]
    fn the_debug_log_goes_in_a_private_state_folder() {
        let dir = test_dir("xdg-debug-log");
        let path = debug_log_file(Some(dir.join("state"))).unwrap();
        assert_eq!(path, dir.join("state").join("audiosync").join("debug.log"));
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert!(debug_log_file(None).is_err());
    }

    #[test]
    fn a_runtime_folder_counts_only_when_private_and_ours() {
        let dir = test_dir("xdg-runtime");
        let user = user_id().unwrap();
        let runtime = dir.join("run");
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            runtime_base(Some(runtime.clone().into()), user),
            Some(runtime.clone())
        );
        assert_eq!(runtime_base(Some(runtime.clone().into()), user + 1), None);
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(runtime_base(Some(runtime.into()), user), None);
        assert_eq!(runtime_base(Some("relative".into()), user), None);
        assert_eq!(runtime_base(None, user), None);
    }

    #[test]
    fn an_owned_folder_open_to_others_is_tightened() {
        let dir = test_dir("xdg-owned-dir");
        let user = user_id().unwrap();
        let folder = dir.join("instance");
        owned_private_dir(&folder, user).unwrap();
        assert_eq!(mode(&folder), 0o700);
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o777)).unwrap();
        owned_private_dir(&folder, user).unwrap();
        assert_eq!(mode(&folder), 0o700);
        assert!(owned_private_dir(&folder, user + 1).is_err());

        let file = dir.join("file");
        fs::write(&file, b"x").unwrap();
        assert!(owned_private_dir(&file, user).is_err());
    }

    #[test]
    fn a_readable_file_is_set_to_owner_only() {
        let dir = test_dir("xdg-tighten");
        let file = dir.join("airplay-pairings.dat");
        assert!(!tighten(&file, 0o600).unwrap());
        fs::write(&file, b"x").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(tighten(&file, 0o600).unwrap());
        assert_eq!(mode(&file), 0o600);
        assert!(!tighten(&file, 0o600).unwrap());
    }

    #[test]
    fn every_base_folder_from_the_environment_is_absolute() {
        for base in [data_base(), state_base(), config_base()] {
            assert!(
                base.as_ref().is_none_or(|base| base.is_absolute()),
                "{base:?}"
            );
        }
    }

    #[test]
    fn the_sealer_stores_the_bytes_as_they_are() {
        let sealer = sealer();
        let sealed = sealer.seal(b"AudioSync pairings 1\n").unwrap();
        assert_eq!(sealed, b"AudioSync pairings 1\n");
        assert_eq!(*sealer.open(&sealed).unwrap(), sealed);
    }
}
