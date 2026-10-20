//! Launch at Sign-In: the Windows counterpart of the Mac's Launch at Login.
//! AudioSync adds itself to the current user's Run key
//! (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, documented in
//! "Run and RunOnce Registry Keys") as a value named `AudioSync` that starts
//! this exe with [`BACKGROUND_FLAG`], and removes that value to turn it off.
//!
//! What the menus show is read again from the registry, never kept in the
//! settings, so a change made in Task Manager or Settings > Apps > Startup
//! shows up. Those two mark an entry turned off under
//! `Explorer\StartupApproved\Run`, a key Microsoft doesn't document: AudioSync
//! only reads it (an entry whose first byte is odd counts as turned off) and
//! never writes it. Turning the option on while Windows has AudioSync turned
//! off there writes the Run value, then says in a notice where to finish and
//! opens that Settings page, as the Mac does when macOS wants approval.
//!
//! A Run value that starts another copy of AudioSync (the exe moved, or
//! another build wrote it) still shows as on. It is rewritten or removed only
//! when the user turns the option on or off: the documentation asks programs
//! started from the Run key not to write to it while they run.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::ui_model::{Notice, NoticeMessage};

/// Started with this, AudioSync opens in the tray without its main window.
pub const BACKGROUND_FLAG: &str = "--background";

/// The longest command line the Run key takes, in UTF-16 units, per its
/// documentation.
pub const MAX_COMMAND_LENGTH: usize = 260;

/// The notice's id. The windows pass notice ids as `i32`; the backend's
/// count from 1 and never come near this one.
pub const NOTICE_ID: u64 = i32::MAX as u64;

/// Where the startup entry lives: the registry on Windows, memory in tests.
pub trait StartupStore {
    /// The Run value's command line, `None` when there is none (or it isn't
    /// a string).
    fn run_value(&self) -> Option<String>;
    fn set_run_value(&mut self, command: &str) -> Result<(), String>;
    /// Removes the Run value; one that isn't there is no error.
    fn remove_run_value(&mut self) -> Result<(), String>;
    /// The entry's StartupApproved data, `None` when Windows has none.
    fn approval(&self) -> Option<Vec<u8>>;
}

/// The startup entry as Windows has it now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// No Run value.
    Off,
    /// The Run value starts this exe.
    On,
    /// The Run value starts another exe: AudioSync moved since it was
    /// written, or another copy wrote it.
    Elsewhere,
    /// Task Manager or Settings turned the entry off.
    TurnedOffInSettings,
}

impl Status {
    /// Whether the menus show the option checked.
    pub fn shown_on(self) -> bool {
        matches!(self, Status::On | Status::Elsewhere)
    }
}

/// The Run value that starts `exe` in the background. Windows paths can't
/// hold a quote, so quoting the path is enough for one with spaces.
pub fn run_command(exe: &Path) -> String {
    format!("\"{}\" {BACKGROUND_FLAG}", exe.display())
}

/// The exe a Run value's command line starts: the quoted path, or what comes
/// before the first space when it isn't quoted.
pub fn command_exe(command: &str) -> Option<PathBuf> {
    let command = command.trim_start();
    let path = match command.strip_prefix('"') {
        Some(rest) => rest.split('"').next()?,
        None => command.split_whitespace().next()?,
    };
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Whether StartupApproved data marks the entry turned off.
pub fn turned_off_in_settings(approval: &[u8]) -> bool {
    approval.first().is_some_and(|flags| flags & 1 == 1)
}

fn same_exe(a: &Path, b: &Path) -> bool {
    let normal = |p: &Path| p.to_string_lossy().replace('/', "\\").to_lowercase();
    normal(a) == normal(b)
}

/// Reads the startup entry for `exe` from `store`.
pub fn status(store: &dyn StartupStore, exe: Option<&Path>) -> Status {
    let Some(command) = store.run_value() else {
        return Status::Off;
    };
    if store
        .approval()
        .is_some_and(|data| turned_off_in_settings(&data))
    {
        return Status::TurnedOffInSettings;
    }
    match (command_exe(&command), exe) {
        (Some(listed), Some(exe)) if same_exe(&listed, exe) => Status::On,
        _ => Status::Elsewhere,
    }
}

/// Whether the command line asks for a start in the background. The first
/// argument is the exe.
pub fn started_in_background<A: AsRef<OsStr>>(args: impl IntoIterator<Item = A>) -> bool {
    args.into_iter()
        .skip(1)
        .any(|arg| arg.as_ref() == OsStr::new(BACKGROUND_FLAG))
}

/// Whether the launch opens the main window. Without a tray icon it is the
/// only way back into the app, so it opens even in the background.
pub fn opens_main_window(background: bool, tray_available: bool) -> bool {
    !background || !tray_available
}

/// Whether a launch while another copy runs asks that copy to show its main
/// window. One in the background (Windows starting the Run key's entry, or
/// reopening apps after a restart) leaves it as it is.
pub fn hands_over_to_running_copy(background: bool) -> bool {
    !background
}

/// What turning the option on or off did, beyond what [`LaunchAtLogin::is_on`]
/// shows.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Why the registry refused, for the log.
    pub error: Option<String>,
    /// Windows has the entry turned off: open Settings > Apps > Startup.
    pub open_startup_settings: bool,
}

/// The option, its last reading and the notice about it.
pub struct LaunchAtLogin {
    store: Box<dyn StartupStore>,
    exe: Option<PathBuf>,
    status: Status,
    notice: Option<Notice>,
}

impl LaunchAtLogin {
    /// `exe` is this program's path, `None` when Windows didn't say.
    pub fn new(store: Box<dyn StartupStore>, exe: Option<PathBuf>) -> LaunchAtLogin {
        let status = status(store.as_ref(), exe.as_deref());
        LaunchAtLogin {
            store,
            exe,
            status,
            notice: None,
        }
    }

    pub fn status(&self) -> Status {
        self.status
    }

    pub fn is_on(&self) -> bool {
        self.status.shown_on()
    }

    pub fn notice(&self) -> Option<&Notice> {
        self.notice.as_ref()
    }

    /// Dismisses the notice if `id` is its id; returns whether it was.
    pub fn dismiss(&mut self, id: u64) -> bool {
        id == NOTICE_ID && self.notice.take().is_some()
    }

    pub fn dismiss_notice(&mut self) {
        self.notice = None;
    }

    /// Reads the entry again; returns whether what the windows show changed.
    /// A notice whose cause went (the user fixed it in Settings) goes too.
    pub fn reread(&mut self) -> bool {
        let new = status(self.store.as_ref(), self.exe.as_deref());
        let mut changed = new != self.status;
        self.status = new;
        if self
            .notice
            .as_ref()
            .is_some_and(|notice| settled(&notice.message, new))
        {
            self.notice = None;
            changed = true;
        }
        changed
    }

    /// The user turned the option on or off; `time` stamps a notice it posts.
    pub fn set(&mut self, on: bool, time: impl FnOnce() -> String) -> Outcome {
        self.notice = None;
        let mut outcome = Outcome::default();
        let message = if on {
            match self.write_run_value() {
                Err(error) => {
                    outcome.error = Some(error);
                    Some(NoticeMessage::LaunchAtLoginFailed)
                }
                Ok(()) => None,
            }
        } else {
            match self.store.remove_run_value() {
                Err(error) => {
                    outcome.error = Some(error);
                    Some(NoticeMessage::LaunchAtLoginNotRemoved)
                }
                Ok(()) => None,
            }
        };
        self.status = status(self.store.as_ref(), self.exe.as_deref());
        let message = message.or_else(|| {
            (on && self.status == Status::TurnedOffInSettings).then(|| {
                outcome.open_startup_settings = true;
                NoticeMessage::LaunchAtLoginOffInSettings
            })
        });
        self.notice = message.map(|message| Notice {
            id: NOTICE_ID,
            subject: "launch at sign-in".to_owned(),
            message,
            time: time(),
        });
        outcome
    }

    fn write_run_value(&mut self) -> Result<(), String> {
        let exe = self
            .exe
            .as_deref()
            .ok_or_else(|| "Windows didn't say where this exe is".to_owned())?;
        let command = run_command(exe);
        let length = command.encode_utf16().count();
        if length > MAX_COMMAND_LENGTH {
            return Err(format!(
                "the command line is {length} characters, over the Run key's {MAX_COMMAND_LENGTH}"
            ));
        }
        self.store.set_run_value(&command)
    }
}

/// Whether the entry now is what the notice asked for.
fn settled(message: &NoticeMessage, status: Status) -> bool {
    match message {
        NoticeMessage::LaunchAtLoginFailed | NoticeMessage::LaunchAtLoginOffInSettings => {
            status == Status::On
        }
        NoticeMessage::LaunchAtLoginNotRemoved => !status.shown_on(),
        _ => false,
    }
}

/// The store the app uses: the registry on Windows, memory elsewhere and in
/// tests, so no test reads or changes the user's startup list.
pub fn system_store() -> Box<dyn StartupStore> {
    #[cfg(all(windows, not(test)))]
    {
        Box::new(registry::RegistryStore)
    }
    #[cfg(any(not(windows), test))]
    {
        Box::new(MemoryStore::default())
    }
}

/// The startup entry kept in memory. Clones share it, so a test can look
/// at what the app wrote.
#[cfg(any(not(windows), test))]
#[derive(Clone, Default)]
pub struct MemoryStore(std::rc::Rc<std::cell::RefCell<MemoryEntry>>);

#[cfg(any(not(windows), test))]
#[derive(Default)]
pub struct MemoryEntry {
    pub run: Option<String>,
    pub approval: Option<Vec<u8>>,
    /// Writes and removals fail, as when a policy locks the key.
    pub refuse_writes: bool,
}

#[cfg(any(not(windows), test))]
impl MemoryStore {
    pub fn entry(&self) -> std::cell::RefMut<'_, MemoryEntry> {
        self.0.borrow_mut()
    }
}

#[cfg(any(not(windows), test))]
impl StartupStore for MemoryStore {
    fn run_value(&self) -> Option<String> {
        self.0.borrow().run.clone()
    }

    fn set_run_value(&mut self, command: &str) -> Result<(), String> {
        let mut entry = self.0.borrow_mut();
        if entry.refuse_writes {
            return Err("access denied".to_owned());
        }
        entry.run = Some(command.to_owned());
        Ok(())
    }

    fn remove_run_value(&mut self) -> Result<(), String> {
        let mut entry = self.0.borrow_mut();
        if entry.refuse_writes {
            return Err("access denied".to_owned());
        }
        entry.run = None;
        Ok(())
    }

    fn approval(&self) -> Option<Vec<u8>> {
        self.0.borrow().approval.clone()
    }
}

#[cfg(all(windows, not(test)))]
mod registry {
    use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ, RegDeleteKeyValueW,
        RegGetValueW, RegSetKeyValueW,
    };
    use windows::core::{PCWSTR, w};

    use super::StartupStore;

    const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
    const APPROVED_KEY: PCWSTR =
        w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run");
    const VALUE_NAME: PCWSTR = w!("AudioSync");

    /// The current user's Run key.
    pub struct RegistryStore;

    /// The size in bytes of a value under `HKEY_CURRENT_USER\key`.
    fn value_size(
        key: PCWSTR,
        flags: windows::Win32::System::Registry::REG_ROUTINE_FLAGS,
    ) -> Option<u32> {
        let mut size = 0u32;
        // SAFETY: a size query: no data buffer, `size` is valid for a write;
        // the key and value names are NUL-terminated UTF-16 literals.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key,
                VALUE_NAME,
                flags,
                None,
                None,
                Some(&raw mut size),
            )
        };
        status.is_ok().then_some(size)
    }

    impl StartupStore for RegistryStore {
        fn run_value(&self) -> Option<String> {
            let size = value_size(RUN_KEY, RRF_RT_REG_SZ)?;
            // Room for a value that grew between the two calls is not
            // needed: a failed second read counts as no value.
            let mut text = vec![0u16; (size as usize).div_ceil(2) + 1];
            let mut size = u32::try_from(text.len() * 2).ok()?;
            // SAFETY: `text` is valid for writes of `size` bytes, and
            // RegGetValueW writes no more (it fails when the value doesn't
            // fit); the names are NUL-terminated UTF-16 literals.
            let status = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    RUN_KEY,
                    VALUE_NAME,
                    RRF_RT_REG_SZ,
                    None,
                    Some(text.as_mut_ptr().cast()),
                    Some(&raw mut size),
                )
            };
            if status.is_err() {
                return None;
            }
            // The size is in bytes and counts the terminating NUL.
            let length = (size as usize / 2).saturating_sub(1).min(text.len());
            Some(String::from_utf16_lossy(&text[..length]))
        }

        fn set_run_value(&mut self, command: &str) -> Result<(), String> {
            let data: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
            let bytes = u32::try_from(data.len() * 2).map_err(|e| e.to_string())?;
            // SAFETY: `data` is a NUL-terminated UTF-16 string of `bytes`
            // bytes that outlives the call; the names are literals.
            let status = unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    RUN_KEY,
                    VALUE_NAME,
                    REG_SZ.0,
                    Some(data.as_ptr().cast()),
                    bytes,
                )
            };
            status.ok().map_err(|e| e.to_string())
        }

        fn remove_run_value(&mut self) -> Result<(), String> {
            // SAFETY: the key and value names are NUL-terminated literals.
            let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE_NAME) };
            if status == ERROR_FILE_NOT_FOUND {
                return Ok(());
            }
            status.ok().map_err(|e| e.to_string())
        }

        fn approval(&self) -> Option<Vec<u8>> {
            let mut data = [0u8; 64];
            let mut size = size_of_val(&data) as u32;
            // SAFETY: `data` is valid for writes of `size` bytes, and
            // RegGetValueW writes no more; the names are literals.
            let status = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    APPROVED_KEY,
                    VALUE_NAME,
                    RRF_RT_REG_BINARY,
                    None,
                    Some(data.as_mut_ptr().cast()),
                    Some(&raw mut size),
                )
            };
            status
                .is_ok()
                .then(|| data[..(size as usize).min(data.len())].to_vec())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe() -> PathBuf {
        PathBuf::from(r"C:\Program Files\AudioSync\audiosync.exe")
    }

    fn setup(entry: impl FnOnce(&mut MemoryEntry)) -> (MemoryStore, LaunchAtLogin) {
        let store = MemoryStore::default();
        entry(&mut store.entry());
        let login = LaunchAtLogin::new(Box::new(store.clone()), Some(exe()));
        (store, login)
    }

    fn time() -> String {
        "09.40".to_owned()
    }

    #[test]
    fn the_command_quotes_the_path_and_asks_for_the_background() {
        assert_eq!(
            run_command(&exe()),
            r#""C:\Program Files\AudioSync\audiosync.exe" --background"#
        );
        assert_eq!(command_exe(&run_command(&exe())), Some(exe()));
    }

    #[test]
    fn a_command_without_quotes_names_what_comes_before_the_first_space() {
        assert_eq!(
            command_exe(r"C:\Tools\audiosync.exe --background"),
            Some(PathBuf::from(r"C:\Tools\audiosync.exe"))
        );
        assert_eq!(command_exe("  "), None);
        assert_eq!(command_exe(r#""""#), None);
    }

    #[test]
    fn no_run_value_reads_as_off() {
        let (_, login) = setup(|_| {});
        assert_eq!(login.status(), Status::Off);
        assert!(!login.is_on());
    }

    #[test]
    fn a_run_value_for_this_exe_reads_as_on_whatever_its_case_or_flags() {
        let (_, login) = setup(|e| {
            e.run = Some(r#""c:\program files\audiosync\AUDIOSYNC.EXE""#.to_owned());
        });
        assert_eq!(login.status(), Status::On);
        assert!(login.is_on());
    }

    #[test]
    fn an_entry_turned_off_in_task_manager_reads_as_off() {
        let (_, login) = setup(|e| {
            e.run = Some(run_command(&exe()));
            e.approval = Some(vec![3, 0, 0, 0, 0, 0, 0, 0, 0x10, 0x20, 0x30, 0x40]);
        });
        assert_eq!(login.status(), Status::TurnedOffInSettings);
        assert!(!login.is_on());
    }

    #[test]
    fn an_entry_approved_in_task_manager_reads_as_on() {
        let (_, login) = setup(|e| {
            e.run = Some(run_command(&exe()));
            e.approval = Some(vec![2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        });
        assert_eq!(login.status(), Status::On);
    }

    #[test]
    fn turning_it_on_writes_the_command_and_turning_it_off_removes_it() {
        let (store, mut login) = setup(|_| {});
        assert_eq!(login.set(true, time), Outcome::default());
        assert_eq!(
            store.entry().run.as_deref(),
            Some(run_command(&exe()).as_str())
        );
        assert!(login.is_on());
        assert!(login.notice().is_none());

        assert_eq!(login.set(false, time), Outcome::default());
        assert_eq!(store.entry().run, None);
        assert!(!login.is_on());
        // Off twice is still fine.
        assert_eq!(login.set(false, time), Outcome::default());
    }

    #[test]
    fn a_moved_exe_shows_on_and_a_toggle_rewrites_or_removes_it() {
        let (store, mut login) = setup(|e| {
            e.run = Some(r#""D:\Old\audiosync.exe" --background"#.to_owned());
        });
        assert_eq!(login.status(), Status::Elsewhere);
        assert!(login.is_on());
        // Nothing is rewritten by reading.
        login.reread();
        assert_eq!(
            store.entry().run.as_deref(),
            Some(r#""D:\Old\audiosync.exe" --background"#)
        );

        login.set(false, time);
        assert_eq!(store.entry().run, None);
        login.set(true, time);
        assert_eq!(
            store.entry().run.as_deref(),
            Some(run_command(&exe()).as_str())
        );
        assert_eq!(login.status(), Status::On);
    }

    #[test]
    fn a_refused_write_posts_a_notice_and_stays_off() {
        let (_, mut login) = setup(|e| e.refuse_writes = true);
        let outcome = login.set(true, time);
        assert_eq!(outcome.error.as_deref(), Some("access denied"));
        assert!(!outcome.open_startup_settings);
        assert!(!login.is_on());
        let notice = login.notice().unwrap();
        assert_eq!(notice.message, NoticeMessage::LaunchAtLoginFailed);
        assert_eq!(notice.id, NOTICE_ID);
        assert_eq!(notice.time, "09.40");
    }

    #[test]
    fn a_refused_removal_posts_its_own_notice_and_stays_on() {
        let (_, mut login) = setup(|e| {
            e.run = Some(run_command(&exe()));
            e.refuse_writes = true;
        });
        login.set(false, time);
        assert!(login.is_on());
        assert_eq!(
            login.notice().unwrap().message,
            NoticeMessage::LaunchAtLoginNotRemoved
        );
    }

    #[test]
    fn the_refused_removal_notice_goes_once_settings_turns_it_off() {
        let (store, mut login) = setup(|e| {
            e.run = Some(run_command(&exe()));
            e.refuse_writes = true;
        });
        login.set(false, time);
        // The notice's advice: turn AudioSync off in Settings > Apps > Startup.
        store.entry().approval = Some(vec![3; 12]);
        assert!(login.reread());
        assert!(!login.is_on());
        assert!(login.notice().is_none());
    }

    #[test]
    fn turning_it_on_while_settings_has_it_off_asks_to_finish_there() {
        let (store, mut login) = setup(|e| e.approval = Some(vec![3; 12]));
        let outcome = login.set(true, time);
        assert!(outcome.open_startup_settings);
        assert_eq!(outcome.error, None);
        // The Run value is written, the approval left alone.
        assert_eq!(
            store.entry().run.as_deref(),
            Some(run_command(&exe()).as_str())
        );
        assert_eq!(store.entry().approval, Some(vec![3; 12]));
        assert!(!login.is_on());
        assert_eq!(
            login.notice().unwrap().message,
            NoticeMessage::LaunchAtLoginOffInSettings
        );

        // The user turns it on in Settings: the next reading clears the notice.
        store.entry().approval = Some(vec![2; 12]);
        assert!(login.reread());
        assert!(login.is_on());
        assert!(login.notice().is_none());
    }

    #[test]
    fn a_change_in_task_manager_shows_on_the_next_reading() {
        let (store, mut login) = setup(|e| e.run = Some(run_command(&exe())));
        assert!(login.is_on());
        assert!(!login.reread());
        store.entry().approval = Some(vec![3; 12]);
        assert!(login.reread());
        assert!(!login.is_on());
    }

    #[test]
    fn a_new_attempt_and_a_dismissal_clear_the_notice() {
        let (store, mut login) = setup(|e| e.refuse_writes = true);
        login.set(true, time);
        assert!(!login.dismiss(1));
        assert!(login.dismiss(NOTICE_ID));
        assert!(login.notice().is_none());
        login.set(true, time);
        store.entry().refuse_writes = false;
        login.set(true, time);
        assert!(login.notice().is_none());
    }

    #[test]
    fn a_command_over_the_run_keys_limit_is_not_written() {
        let store = MemoryStore::default();
        let long = PathBuf::from(format!(r"C:\{}\audiosync.exe", "a".repeat(250)));
        let mut login = LaunchAtLogin::new(Box::new(store.clone()), Some(long));
        let outcome = login.set(true, time);
        assert!(outcome.error.unwrap().contains("over the Run key's 260"));
        assert_eq!(store.entry().run, None);
        assert_eq!(
            login.notice().unwrap().message,
            NoticeMessage::LaunchAtLoginFailed
        );
    }

    #[test]
    fn without_its_own_path_it_cant_turn_on() {
        let store = MemoryStore::default();
        let mut login = LaunchAtLogin::new(Box::new(store.clone()), None);
        assert!(login.set(true, time).error.is_some());
        assert_eq!(store.entry().run, None);
    }

    #[test]
    fn the_background_flag_opens_no_window_only_with_a_tray() {
        assert!(started_in_background(["audiosync.exe", "--background"]));
        assert!(!started_in_background(["audiosync.exe"]));
        // The exe's own name is not an argument.
        assert!(!started_in_background(["--background"]));
        assert!(!opens_main_window(true, true));
        assert!(opens_main_window(true, false));
        assert!(opens_main_window(false, true));
        assert!(opens_main_window(false, false));
    }

    #[test]
    fn a_second_launch_in_the_background_leaves_the_running_copy_alone() {
        assert!(!hands_over_to_running_copy(true));
        assert!(hands_over_to_running_copy(false));
    }
}
