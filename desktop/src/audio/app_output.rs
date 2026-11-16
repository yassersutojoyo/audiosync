//! Moving one captured app's own output to a silent endpoint while it
//! streams (S471), so that app goes quiet on the PC while the system default
//! and every other app stay where they are. Process loopback keeps taking the
//! app in at full level on the silent endpoint (hardware test 2), whereas a
//! muted session gives it silence (hardware test 1).
//!
//! Windows keeps this per-app choice per executable and across restarts, so
//! the app's earlier choice goes to a restore record before the first change
//! and comes back per app; nothing here ever clears every app's choice. The
//! decisions run over the [`AppEndpoints`] and [`AppRecordStore`] seams,
//! which tests drive with fakes.

use serde::{Deserialize, Serialize};

use super::default_output::{ROLES, Role};

/// The first build with process loopback, below which nothing is redirected.
pub const PROCESS_LOOPBACK_BUILD: u32 = 20_348;
/// From this build the per-app interface has its later IID and takes the
/// device as an HSTRING.
pub const LATER_INTERFACE_BUILD: u32 = 21_390;

/// The per-app interface's variant for an OS build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    Earlier,
    Later,
}

/// The variant to use on `build`, or `None` when the build is too old to
/// redirect at all.
pub fn variant(build: u32) -> Option<Variant> {
    match build {
        ..PROCESS_LOOPBACK_BUILD => None,
        PROCESS_LOOPBACK_BUILD..LATER_INTERFACE_BUILD => Some(Variant::Earlier),
        _ => Some(Variant::Later),
    }
}

/// A render endpoint ID in the device-interface form the per-app calls take
/// and return.
pub fn render_interface(endpoint_id: &str) -> String {
    format!(r"\\?\SWD#MMDEVAPI#{endpoint_id}#{{e6327cad-dcec-4949-ae8a-991e976a79d2}}")
}

/// Sets `role` for `pid`'s app and reads it back: a success code alone
/// doesn't prove the change.
fn moved(
    endpoints: &mut dyn AppEndpoints,
    pid: u32,
    role: Role,
    device: &str,
) -> Result<(), String> {
    endpoints.set_app_default(pid, role, device)?;
    match endpoints.app_default(pid, role)? {
        now if now == device => Ok(()),
        now => Err(format!(
            "the call succeeded but the app's output reads {now:?}"
        )),
    }
}

/// One app's own output per role before the redirect; empty is "Default".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Previous {
    pub exe: String,
    pub console: String,
    pub multimedia: String,
}

impl Previous {
    fn get(&self, role: Role) -> &str {
        match role {
            Role::Console => &self.console,
            Role::Multimedia => &self.multimedia,
        }
    }
}

/// What a restore needs: the silent endpoint in device-interface form and
/// each redirected app's earlier choice.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppRecord {
    pub silent: String,
    pub apps: Vec<Previous>,
}

/// A running process of the captured tree, or one a restore can reach its
/// executable through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub exe: String,
}

/// Windows' per-app output, reached through one of the app's processes.
pub trait AppEndpoints {
    /// The app's own output for `role` in device-interface form; empty when
    /// it follows the default.
    fn app_default(&mut self, pid: u32, role: Role) -> Result<String, String>;
    /// Sets the app's own output for `role`; empty puts it back on the
    /// default.
    fn set_app_default(&mut self, pid: u32, role: Role, device: &str) -> Result<(), String>;
}

/// Where the restore record lives: the settings file.
pub trait AppRecordStore {
    fn load(&mut self) -> Result<Option<AppRecord>, String>;
    /// Writes `record`, or deletes the one there with `None`.
    fn save(&mut self, record: Option<&AppRecord>) -> Result<(), String>;
}

/// The redirect across a stream, and the record as on disk.
#[derive(Debug, Default)]
pub struct AppRedirect {
    record: Option<AppRecord>,
}

impl AppRedirect {
    /// The record the settings file holds.
    pub fn record(&self) -> Option<&AppRecord> {
        self.record.as_ref()
    }

    /// Moves every app in `processes` not moved yet to `silent_endpoint`,
    /// for the console and multimedia roles.
    pub fn redirect(
        &mut self,
        endpoints: &mut dyn AppEndpoints,
        store: &mut dyn AppRecordStore,
        silent_endpoint: &str,
        processes: &[Process],
    ) -> Vec<String> {
        let mut record = self.record.clone().unwrap_or_else(|| AppRecord {
            silent: render_interface(silent_endpoint),
            apps: Vec::new(),
        });
        let mut lines = Vec::new();
        for process in processes {
            if record.apps.iter().any(|app| app.exe == process.exe) {
                continue;
            }
            let exe = &process.exe;
            let read =
                |endpoints: &mut dyn AppEndpoints, role| endpoints.app_default(process.pid, role);
            let previous = match (
                read(endpoints, Role::Console),
                read(endpoints, Role::Multimedia),
            ) {
                (Ok(console), Ok(multimedia)) => Previous {
                    exe: exe.clone(),
                    console,
                    multimedia,
                },
                (Err(error), _) | (_, Err(error)) => {
                    lines.push(format!(
                        "app output: {exe} not moved, it can't be read: {error}"
                    ));
                    continue;
                }
            };
            record.apps.push(previous.clone());
            if let Err(error) = self.save(store, &record) {
                record.apps.pop();
                lines.push(format!(
                    "app output: {exe} not moved, the restore record couldn't be written: {error}"
                ));
                continue;
            }
            let failed = ROLES.iter().enumerate().find_map(|(done, &role)| {
                moved(endpoints, process.pid, role, &record.silent)
                    .err()
                    .map(|error| (done, role, error))
            });
            let Some((done, role, error)) = failed else {
                lines.push(format!("app output: {exe} moved to {}", record.silent));
                continue;
            };
            lines.push(format!(
                "app output: {exe} not moved, {role:?} failed: {error}"
            ));
            let undone = ROLES[..done]
                .iter()
                .all(|&role| moved(endpoints, process.pid, role, previous.get(role)).is_ok());
            if undone {
                record.apps.pop();
                if let Err(error) = self.save(store, &record) {
                    lines.push(format!(
                        "app output: the restore record couldn't be written: {error}"
                    ));
                }
            } else {
                lines.push(format!(
                    "app output: {exe} couldn't be put back, the record stays for the next launch"
                ));
            }
        }
        lines
    }

    fn save(&mut self, store: &mut dyn AppRecordStore, record: &AppRecord) -> Result<(), String> {
        let kept = (!record.apps.is_empty()).then_some(record);
        store.save(kept)?;
        self.record = kept.cloned();
        Ok(())
    }

    /// Puts each recorded app back through one of its `running` processes.
    pub fn restore(
        &mut self,
        endpoints: &mut dyn AppEndpoints,
        store: &mut dyn AppRecordStore,
        running: &[Process],
    ) -> Vec<String> {
        let Some(mut record) = self.record.clone() else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        record.apps.retain(|app| {
            let Some(process) = running.iter().find(|process| process.exe == app.exe) else {
                lines.push(format!(
                    "app output: {} isn't running, it is put back when it runs again",
                    app.exe
                ));
                return true;
            };
            let mut kept = false;
            for role in ROLES {
                match endpoints.app_default(process.pid, role) {
                    Ok(now) if now == record.silent => {
                        if let Err(error) = moved(endpoints, process.pid, role, app.get(role)) {
                            kept = true;
                            lines.push(format!(
                                "app output: {} {role:?} couldn't be put back: {error}",
                                app.exe
                            ));
                        }
                    }
                    Ok(_) => lines.push(format!(
                        "app output: {} {role:?} left as it is, it was changed meanwhile",
                        app.exe
                    )),
                    Err(error) => {
                        kept = true;
                        lines.push(format!(
                            "app output: {} {role:?} can't be read: {error}",
                            app.exe
                        ));
                    }
                }
            }
            if !kept {
                lines.push(format!("app output: {} put back", app.exe));
            }
            kept
        });
        if let Err(error) = self.save(store, &record) {
            lines.push(format!(
                "app output: the restore record couldn't be written: {error}"
            ));
        }
        lines
    }

    /// At launch, before streaming: restores what a record on disk says.
    pub fn restore_at_launch(
        &mut self,
        endpoints: &mut dyn AppEndpoints,
        store: &mut dyn AppRecordStore,
        running: &[Process],
    ) -> Vec<String> {
        match store.load() {
            Ok(None) => Vec::new(),
            Ok(Some(record)) => {
                self.record = Some(record);
                self.restore(endpoints, store, running)
            }
            Err(error) => vec![format!(
                "app output: the restore record couldn't be read: {error}"
            )],
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    use super::*;

    const SILENT: &str = "{0.0.0.00000000}.{silent}";

    pub(crate) type Calls = Rc<RefCell<Vec<String>>>;

    /// Windows' per-app outputs, keyed by executable and role.
    pub(crate) struct Fake {
        calls: Calls,
        exes: BTreeMap<u32, String>,
        outputs: BTreeMap<(String, &'static str), String>,
        failing: Option<Role>,
        ignored: Option<Role>,
    }

    fn key(role: Role) -> &'static str {
        match role {
            Role::Console => "console",
            Role::Multimedia => "multimedia",
        }
    }

    impl Fake {
        pub(crate) fn new(calls: &Calls, processes: &[Process]) -> Fake {
            Fake {
                calls: Rc::clone(calls),
                exes: processes.iter().map(|p| (p.pid, p.exe.clone())).collect(),
                outputs: BTreeMap::new(),
                failing: None,
                ignored: None,
            }
        }

        fn exe(&self, pid: u32) -> Result<String, String> {
            self.exes.get(&pid).cloned().ok_or("no such process".into())
        }

        pub(crate) fn output(&self, exe: &str, role: Role) -> String {
            let key = (exe.to_owned(), key(role));
            self.outputs.get(&key).cloned().unwrap_or_default()
        }

        pub(crate) fn put(&mut self, exe: &str, role: Role, device: &str) {
            self.outputs
                .insert((exe.to_owned(), key(role)), device.to_owned());
        }
    }

    impl AppEndpoints for Fake {
        fn app_default(&mut self, pid: u32, role: Role) -> Result<String, String> {
            let exe = self.exe(pid)?;
            Ok(self.output(&exe, role))
        }

        fn set_app_default(&mut self, pid: u32, role: Role, device: &str) -> Result<(), String> {
            let exe = self.exe(pid)?;
            self.calls
                .borrow_mut()
                .push(format!("set {exe} {} {device}", key(role)));
            if self.failing == Some(role) {
                return Err("E_FAIL".into());
            }
            if self.ignored != Some(role) {
                self.put(&exe, role, device);
            }
            Ok(())
        }
    }

    pub(crate) struct Store {
        pub(crate) calls: Calls,
        pub(crate) record: Option<AppRecord>,
        pub(crate) failing: bool,
    }

    impl AppRecordStore for Store {
        fn load(&mut self) -> Result<Option<AppRecord>, String> {
            Ok(self.record.clone())
        }

        fn save(&mut self, record: Option<&AppRecord>) -> Result<(), String> {
            self.calls.borrow_mut().push(format!(
                "save {}",
                record.map_or(0, |record| record.apps.len())
            ));
            if self.failing {
                return Err("disk full".into());
            }
            self.record = record.cloned();
            Ok(())
        }
    }

    pub(crate) fn process(pid: u32, exe: &str) -> Process {
        Process {
            pid,
            exe: exe.to_owned(),
        }
    }

    fn spotify() -> Vec<Process> {
        vec![
            process(10, "C:/Apps/Spotify.exe"),
            process(11, "C:/Apps/Spotify.exe"),
        ]
    }

    fn world(processes: &[Process]) -> (Calls, Fake, Store) {
        let calls = Calls::default();
        let fake = Fake::new(&calls, processes);
        let store = Store {
            calls: Rc::clone(&calls),
            record: None,
            failing: false,
        };
        (calls, fake, store)
    }

    fn silent() -> String {
        render_interface(SILENT)
    }

    #[test]
    fn the_device_is_wrapped_as_a_render_interface_path() {
        assert_eq!(
            render_interface("{0.0.0.00000000}.{abc}"),
            r"\\?\SWD#MMDEVAPI#{0.0.0.00000000}.{abc}#{e6327cad-dcec-4949-ae8a-991e976a79d2}"
        );
    }

    #[test]
    fn the_variant_follows_the_build_and_old_builds_never_redirect() {
        assert_eq!(variant(19_045), None);
        assert_eq!(variant(PROCESS_LOOPBACK_BUILD), Some(Variant::Earlier));
        assert_eq!(variant(21_389), Some(Variant::Earlier));
        assert_eq!(variant(LATER_INTERFACE_BUILD), Some(Variant::Later));
        assert_eq!(variant(26_300), Some(Variant::Later));
    }

    // covers: S471
    #[test]
    fn the_record_is_written_before_the_first_change_and_one_exe_moves_once() {
        let (calls, mut fake, mut store) = world(&spotify());
        let mut redirect = AppRedirect::default();
        redirect.redirect(&mut fake, &mut store, SILENT, &spotify());
        let silent = silent();
        assert_eq!(
            *calls.borrow(),
            vec![
                "save 1".to_owned(),
                format!("set C:/Apps/Spotify.exe console {silent}"),
                format!("set C:/Apps/Spotify.exe multimedia {silent}"),
            ]
        );
        assert_eq!(fake.output("C:/Apps/Spotify.exe", Role::Console), silent);
        let record = store.record.clone().expect("a record");
        assert_eq!(record.silent, silent);
        assert_eq!(
            record.apps,
            vec![Previous {
                exe: "C:/Apps/Spotify.exe".into(),
                console: String::new(),
                multimedia: String::new(),
            }]
        );
    }

    #[test]
    fn a_record_that_cant_be_written_moves_nothing() {
        let (calls, mut fake, mut store) = world(&spotify());
        store.failing = true;
        let lines = AppRedirect::default().redirect(&mut fake, &mut store, SILENT, &spotify());
        assert!(!calls.borrow().iter().any(|call| call.starts_with("set")));
        assert!(lines.iter().any(|line| line.contains("record")));
    }

    #[test]
    fn a_stop_puts_back_the_apps_own_earlier_choice() {
        let (_, mut fake, mut store) = world(&spotify());
        fake.put("C:/Apps/Spotify.exe", Role::Console, "headphones");
        let mut redirect = AppRedirect::default();
        redirect.redirect(&mut fake, &mut store, SILENT, &spotify());
        redirect.restore(&mut fake, &mut store, &spotify());
        assert_eq!(
            fake.output("C:/Apps/Spotify.exe", Role::Console),
            "headphones"
        );
        assert_eq!(fake.output("C:/Apps/Spotify.exe", Role::Multimedia), "");
        assert_eq!(store.record, None);
        assert_eq!(redirect.record(), None);
    }

    // covers: S471
    #[test]
    fn another_apps_choice_is_the_same_before_start_and_after_stop() {
        let mut running = spotify();
        running.push(process(20, "C:/Games/Game.exe"));
        let (_, mut fake, mut store) = world(&running);
        fake.put("C:/Games/Game.exe", Role::Console, "headphones");
        let before = fake.outputs.clone();
        let mut redirect = AppRedirect::default();
        redirect.redirect(&mut fake, &mut store, SILENT, &spotify());
        redirect.restore(&mut fake, &mut store, &running);
        assert_eq!(
            fake.outputs.get(&("C:/Games/Game.exe".into(), "console")),
            before.get(&("C:/Games/Game.exe".into(), "console"))
        );
        assert_eq!(fake.output("C:/Games/Game.exe", Role::Multimedia), "");
    }

    #[test]
    fn a_change_that_doesnt_read_back_is_undone() {
        let (_, mut fake, mut store) = world(&spotify());
        fake.ignored = Some(Role::Multimedia);
        let mut redirect = AppRedirect::default();
        let lines = redirect.redirect(&mut fake, &mut store, SILENT, &spotify());
        assert_eq!(fake.output("C:/Apps/Spotify.exe", Role::Console), "");
        assert_eq!(store.record, None);
        assert!(lines.iter().any(|line| line.contains("Multimedia")));
    }

    #[test]
    fn a_child_that_starts_later_is_moved_too() {
        let mut running = spotify();
        running.push(process(12, "C:/Apps/SpotifyHelper.exe"));
        let (_, mut fake, mut store) = world(&running);
        let mut redirect = AppRedirect::default();
        redirect.redirect(&mut fake, &mut store, SILENT, &spotify());
        redirect.redirect(&mut fake, &mut store, SILENT, &running);
        assert_eq!(
            fake.output("C:/Apps/SpotifyHelper.exe", Role::Multimedia),
            silent()
        );
        assert_eq!(
            store.record.as_ref().map(|record| record.apps.len()),
            Some(2)
        );
    }

    #[test]
    fn a_role_the_user_changed_meanwhile_is_left_alone() {
        let (_, mut fake, mut store) = world(&spotify());
        let mut redirect = AppRedirect::default();
        redirect.redirect(&mut fake, &mut store, SILENT, &spotify());
        fake.put("C:/Apps/Spotify.exe", Role::Console, "speakers");
        redirect.restore(&mut fake, &mut store, &spotify());
        assert_eq!(
            fake.output("C:/Apps/Spotify.exe", Role::Console),
            "speakers"
        );
        assert_eq!(fake.output("C:/Apps/Spotify.exe", Role::Multimedia), "");
        assert_eq!(store.record, None);
    }

    #[test]
    fn an_app_that_isnt_running_stays_recorded_until_it_runs_again() {
        let (_, mut fake, mut store) = world(&spotify());
        let mut redirect = AppRedirect::default();
        redirect.redirect(&mut fake, &mut store, SILENT, &spotify());
        redirect.restore(&mut fake, &mut store, &[]);
        assert_eq!(
            store.record.as_ref().map(|record| record.apps.len()),
            Some(1)
        );
        redirect.restore(&mut fake, &mut store, &spotify());
        assert_eq!(fake.output("C:/Apps/Spotify.exe", Role::Console), "");
        assert_eq!(store.record, None);
    }

    #[test]
    fn the_next_launch_restores_a_record_left_on_disk() {
        let (_, mut fake, mut store) = world(&spotify());
        AppRedirect::default().redirect(&mut fake, &mut store, SILENT, &spotify());
        let mut relaunched = AppRedirect::default();
        relaunched.restore_at_launch(&mut fake, &mut store, &spotify());
        assert_eq!(fake.output("C:/Apps/Spotify.exe", Role::Console), "");
        assert_eq!(fake.output("C:/Apps/Spotify.exe", Role::Multimedia), "");
        assert_eq!(store.record, None);
    }

    #[test]
    fn a_failed_restore_keeps_the_record() {
        let (_, mut fake, mut store) = world(&spotify());
        let mut redirect = AppRedirect::default();
        redirect.redirect(&mut fake, &mut store, SILENT, &spotify());
        fake.failing = Some(Role::Multimedia);
        redirect.restore(&mut fake, &mut store, &spotify());
        assert_eq!(
            store.record.as_ref().map(|record| record.apps.len()),
            Some(1)
        );
    }

    /// A fake shared with the test once the controller holds it.
    pub(crate) struct Shared<T>(pub(crate) Rc<RefCell<T>>);

    impl<T> Clone for Shared<T> {
        fn clone(&self) -> Self {
            Shared(Rc::clone(&self.0))
        }
    }

    impl<T: AppEndpoints> AppEndpoints for Shared<T> {
        fn app_default(&mut self, pid: u32, role: Role) -> Result<String, String> {
            self.0.borrow_mut().app_default(pid, role)
        }

        fn set_app_default(&mut self, pid: u32, role: Role, device: &str) -> Result<(), String> {
            self.0.borrow_mut().set_app_default(pid, role, device)
        }
    }

    impl<T: AppRecordStore> AppRecordStore for Shared<T> {
        fn load(&mut self) -> Result<Option<AppRecord>, String> {
            self.0.borrow_mut().load()
        }

        fn save(&mut self, record: Option<&AppRecord>) -> Result<(), String> {
            self.0.borrow_mut().save(record)
        }
    }
}
