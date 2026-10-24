//! Launch at Login on Linux: the Desktop Application Autostart entry
//! `$XDG_CONFIG_HOME/autostart/<APP_ID>.desktop`. A file of that name in
//! the user's folder overrides any in the system folders, so AudioSync owns
//! exactly this one and writes it whole.
//!
//! A desktop that turned the entry off marks it `Hidden=true` (the
//! specification's way) or `X-GNOME-Autostart-enabled=false` (GNOME's own
//! startup tool); both are only read. Turning the option on rewrites the
//! file without them, so it is simply on again.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::data_folder;
use crate::launch_at_login::{BACKGROUND_FLAG, StartupStore};
use crate::os::desktop_entry::{bool_value, escape_value, exec_command, exec_exe, raw_value};

use super::APP_ID;

/// The autostart entry at one path.
pub struct AutostartStore {
    path: Option<PathBuf>,
}

impl AutostartStore {
    /// The entry in the user's autostart folder; with no config folder
    /// known, reads find nothing and writes fail.
    pub fn system() -> AutostartStore {
        AutostartStore {
            path: super::config_base()
                .map(|base| base.join("autostart").join(format!("{APP_ID}.desktop"))),
        }
    }

    #[cfg(test)]
    pub fn at(path: PathBuf) -> AutostartStore {
        AutostartStore { path: Some(path) }
    }

    fn read(&self) -> Option<String> {
        fs::read_to_string(self.path.as_ref()?).ok()
    }

    fn path(&self) -> Result<&Path, String> {
        self.path
            .as_deref()
            .ok_or_else(|| "neither XDG_CONFIG_HOME nor a home folder is set".to_owned())
    }
}

/// The whole entry for the Exec value `command`.
fn entry(command: &str) -> Result<String, String> {
    let exe = exec_exe(command).ok_or_else(|| format!("no program in {command}"))?;
    let exe = exe
        .to_str()
        .ok_or_else(|| "the program's path isn't UTF-8".to_owned())?;
    Ok(format!(
        "[Desktop Entry]\nType=Application\nVersion=1.5\nName=AudioSync\nExec={}\n\
         TryExec={}\nIcon={APP_ID}\nTerminal=false\n",
        escape_value(command),
        escape_value(exe),
    ))
}

impl StartupStore for AutostartStore {
    fn run_value(&self) -> Option<String> {
        let file = self.read()?;
        raw_value(&file, "Exec").map(crate::os::desktop_entry::unescape_value)
    }

    fn set_run_value(&mut self, command: &str) -> Result<(), String> {
        let path = self.path()?;
        data_folder::write_atomically(path, entry(command)?.as_bytes())
            .map_err(|error| format!("{}: {error}", path.display()))
    }

    fn remove_run_value(&mut self) -> Result<(), String> {
        let path = self.path()?;
        match fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                Err(format!("{}: {error}", path.display()))
            }
            _ => Ok(()),
        }
    }

    /// `[1]` when the desktop turned the entry off, `[0]` when not.
    fn approval(&self) -> Option<Vec<u8>> {
        let file = self.read()?;
        let off = bool_value(&file, "Hidden") == Some(true)
            || bool_value(&file, "X-GNOME-Autostart-enabled") == Some(false);
        Some(vec![u8::from(off)])
    }

    fn command(&self, exe: &Path) -> Result<String, String> {
        exec_command(exe, &[BACKGROUND_FLAG])
    }

    fn command_exe(&self, command: &str) -> Option<PathBuf> {
        exec_exe(command)
    }

    fn same_exe(&self, a: &Path, b: &Path) -> bool {
        a == b
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;
    use crate::debug_log::test_dir;
    use crate::launch_at_login::{LaunchAtLogin, Status};

    fn time() -> String {
        "09.40".to_owned()
    }

    #[test]
    fn turning_it_on_writes_an_entry_that_reads_back_as_on() {
        let dir = test_dir("autostart-on");
        let path = dir.join("autostart").join(format!("{APP_ID}.desktop"));
        let exe = PathBuf::from(r#"/home/me/My "Apps"/$100%/back\slash/AudioSync.AppImage"#);
        let mut login = LaunchAtLogin::new(Box::new(AutostartStore::at(path.clone())), Some(exe));
        assert_eq!(login.status(), Status::Off);

        let outcome = login.set(true, time);
        assert_eq!(outcome.error, None);
        assert!(!outcome.open_startup_settings);
        assert_eq!(login.status(), Status::On);
        assert_eq!(login.notice(), None);

        let file = fs::read_to_string(&path).unwrap();
        assert!(
            file.starts_with("[Desktop Entry]\nType=Application\n"),
            "{file}"
        );
        assert!(file.contains("\nName=AudioSync\n"));
        assert!(file.contains(
            r#"Exec="/home/me/My \\"Apps\\"/\\$100%%/back\\\\slash/AudioSync.AppImage" --background"#
        ));
        assert!(
            file.contains(r#"TryExec=/home/me/My "Apps"/$100%/back\\slash/AudioSync.AppImage"#)
        );
        assert!(!file.contains("Hidden") && !file.contains("X-GNOME"));
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );

        let again = LaunchAtLogin::new(
            Box::new(AutostartStore::at(path.clone())),
            Some(PathBuf::from("/usr/bin/audiosync")),
        );
        assert_eq!(again.status(), Status::Elsewhere);

        login.set(false, time);
        assert!(!path.exists());
        assert_eq!(login.status(), Status::Off);
    }

    #[test]
    fn an_entry_the_desktop_turned_off_reads_as_off_until_turned_on_here() {
        let dir = test_dir("autostart-hidden");
        let exe = PathBuf::from("/opt/audiosync/audiosync");
        for marker in ["Hidden=true", "X-GNOME-Autostart-enabled=false"] {
            let path = dir.join(format!("{}.desktop", marker.len()));
            let mut login = LaunchAtLogin::new(
                Box::new(AutostartStore::at(path.clone())),
                Some(exe.clone()),
            );
            login.set(true, time);
            let file = fs::read_to_string(&path).unwrap();
            fs::write(&path, format!("{file}{marker}\n")).unwrap();
            login.reread();
            assert_eq!(login.status(), Status::TurnedOffInSettings, "{marker}");

            let outcome = login.set(true, time);
            assert_eq!(login.status(), Status::On, "{marker}");
            assert!(!outcome.open_startup_settings);
            assert!(!fs::read_to_string(&path).unwrap().contains(marker));
        }
    }

    #[test]
    fn the_users_entry_is_named_after_the_app_id() {
        if let Some(path) = AutostartStore::system().path {
            assert!(path.is_absolute(), "{path:?}");
            assert!(path.ends_with(format!("autostart/{APP_ID}.desktop")));
        }
    }

    #[test]
    fn a_path_an_exec_key_cant_hold_is_not_written() {
        let dir = test_dir("autostart-refused");
        let path = dir.join(format!("{APP_ID}.desktop"));
        let mut login = LaunchAtLogin::new(
            Box::new(AutostartStore::at(path.clone())),
            Some(PathBuf::from("/opt/a=b/audiosync")),
        );
        assert!(login.set(true, time).error.is_some());
        assert!(!path.exists());
        assert_eq!(login.status(), Status::Off);
    }
}
