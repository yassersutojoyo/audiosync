//! Moving Windows' default output to a silent endpoint while the whole
//! system streams (S469), so apps play where nobody listens and the user
//! hears only AudioSync's copy, and the fallback when it can't move (S470):
//! then the default output gets no copy, since the original plays there.
//!
//! Only the console and multimedia roles move ([`Role`] has no
//! communications role). A restore record goes to the settings file before
//! the first change, so a crash is put right at the next launch. The
//! decisions run over the [`DefaultEndpoints`] and [`RecordStore`] seams,
//! which tests drive with fakes; no test changes the real default output.

use serde::{Deserialize, Serialize};

use super::backend::OutputKind;

/// A default-output role AudioSync may move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Console,
    Multimedia,
}

/// The roles in the order they move: the first is put back when the second
/// fails.
pub const ROLES: [Role; 2] = [Role::Console, Role::Multimedia];

/// What a restore needs: each role's default before the switch, and the
/// silent endpoint it moved to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub console: String,
    pub multimedia: String,
    pub silent: String,
}

impl Record {
    fn previous(&self, role: Role) -> &str {
        match role {
            Role::Console => &self.console,
            Role::Multimedia => &self.multimedia,
        }
    }
}

/// An active render endpoint, as the choice of a silent one reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub kind: OutputKind,
    /// Whether something records the same device's input side, as with a
    /// loopback cable an app listens to: such an endpoint is taken last.
    pub listened: bool,
    /// Whether it holds Windows' communications role, where a call plays:
    /// such an endpoint is never the silent one (S486).
    pub communications: bool,
}

/// Windows' default output per role, and the active render endpoints.
pub trait DefaultEndpoints: Send {
    /// The role's default endpoint ID; `None` when no endpoint has the role.
    fn default_id(&mut self, role: Role) -> Result<Option<String>, String>;
    /// Makes `id` the role's default.
    fn set_default(&mut self, id: &str, role: Role) -> Result<(), String>;
    /// The active render endpoints.
    fn endpoints(&mut self) -> Result<Vec<Candidate>, String>;
}

/// Where the restore record lives: the settings file.
pub trait RecordStore: Send {
    fn load(&mut self) -> Result<Option<Record>, String>;
    /// Writes `record`, or deletes the one there with `None`.
    fn save(&mut self, record: Option<&Record>) -> Result<(), String>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum State {
    /// The whole system isn't streaming.
    #[default]
    Off,
    /// Streaming with the default left alone, since it already is silent or
    /// there is none.
    NotNeeded,
    Switched(Record),
    /// Streaming without the switch: the default output gets no copy (S470).
    Fallback,
}

/// The switch's state across a stream, and the record as on disk.
#[derive(Debug, Default)]
pub struct DefaultSwitch {
    state: State,
    on_disk: Option<Record>,
}

/// The silent endpoint among `endpoints`: a virtual one that isn't among
/// the `chosen` outputs, one nobody listens to first, then the smallest ID.
pub(crate) fn silent_endpoint<'a>(endpoints: &'a [Candidate], chosen: &[&str]) -> Option<&'a str> {
    endpoints
        .iter()
        .filter(|candidate| {
            candidate.kind == OutputKind::Virtual
                && !candidate.communications
                && !chosen.contains(&candidate.id.as_str())
        })
        .min_by(|a, b| (a.listened, &a.id).cmp(&(b.listened, &b.id)))
        .map(|candidate| candidate.id.as_str())
}

/// Whether `role`'s default reads back as `id`.
fn moved(endpoints: &mut dyn DefaultEndpoints, id: &str, role: Role) -> Result<(), String> {
    endpoints.set_default(id, role)?;
    match endpoints.default_id(role)? {
        Some(now) if now == id => Ok(()),
        now => Err(format!(
            "the call succeeded but the default reads {}",
            now.as_deref().unwrap_or("none")
        )),
    }
}

impl DefaultSwitch {
    /// The record the settings file holds, for the adapter's own saves.
    pub fn record(&self) -> Option<&Record> {
        self.on_disk.as_ref()
    }

    /// Whether the default output gets no copy now (S470).
    pub fn holds_out_default(&self) -> bool {
        self.state == State::Fallback
    }

    /// Whether the switch is in effect.
    pub fn switched(&self) -> bool {
        matches!(self.state, State::Switched(_))
    }

    /// Whether a whole-system stream has been handled since it started.
    pub fn engaged(&self) -> bool {
        self.state != State::Off
    }

    fn save(&mut self, store: &mut dyn RecordStore, record: Option<&Record>) -> Result<(), String> {
        store.save(record)?;
        self.on_disk = record.cloned();
        Ok(())
    }

    /// At launch, before streaming: restores what a record on disk says.
    pub fn restore_at_launch(
        &mut self,
        endpoints: &mut dyn DefaultEndpoints,
        store: &mut dyn RecordStore,
    ) -> Vec<String> {
        match store.load() {
            Ok(None) => Vec::new(),
            Ok(Some(record)) => {
                self.on_disk = Some(record.clone());
                let mut lines = vec!["default output: a restore record is on disk".to_owned()];
                lines.extend(self.put_back(endpoints, store, &record, false));
                lines
            }
            Err(error) => vec![format!(
                "default output: the restore record couldn't be read: {error}"
            )],
        }
    }

    /// When the whole system starts streaming to the outputs `chosen`.
    pub fn engage(
        &mut self,
        endpoints: &mut dyn DefaultEndpoints,
        store: &mut dyn RecordStore,
        chosen: &[&str],
    ) -> Vec<String> {
        match self.try_engage(endpoints, store, chosen) {
            Ok(line) => vec![line],
            Err(mut lines) => {
                self.state = State::Fallback;
                lines.push(
                    "default output: Windows' default output gets no copy while this stream runs"
                        .to_owned(),
                );
                lines
            }
        }
    }

    fn try_engage(
        &mut self,
        endpoints: &mut dyn DefaultEndpoints,
        store: &mut dyn RecordStore,
        chosen: &[&str],
    ) -> Result<String, Vec<String>> {
        let not_moved = |why: String| vec![format!("default output: not moved, {why}")];
        let listed = endpoints
            .endpoints()
            .map_err(|error| not_moved(format!("the endpoints couldn't be listed: {error}")))?;
        let Some(silent) = silent_endpoint(&listed, chosen).map(str::to_owned) else {
            return Err(not_moved("no silent endpoint".to_owned()));
        };
        let mut previous = Vec::new();
        for role in ROLES {
            match endpoints.default_id(role) {
                // A role a failed restore left on the silent endpoint keeps
                // the default the record on disk has for it.
                Ok(Some(id)) => match &self.on_disk {
                    Some(kept) if id == kept.silent => {
                        previous.push(kept.previous(role).to_owned())
                    }
                    _ => previous.push(id),
                },
                Ok(None) => {
                    self.state = State::NotNeeded;
                    return Ok(format!("default output: not moved, no {role:?} default"));
                }
                Err(error) => {
                    return Err(not_moved(format!(
                        "the {role:?} default can't be read: {error}"
                    )));
                }
            }
        }
        if previous.iter().all(|id| *id == silent) {
            self.state = State::NotNeeded;
            return Ok(format!(
                "default output: not moved, it already is the silent endpoint {silent}"
            ));
        }
        let record = Record {
            multimedia: previous.pop().unwrap_or_default(),
            console: previous.pop().unwrap_or_default(),
            silent,
        };
        self.save(store, Some(&record)).map_err(|error| {
            not_moved(format!("the restore record couldn't be written: {error}"))
        })?;
        if let Err(error) = moved(endpoints, &record.silent, Role::Console) {
            let mut lines = not_moved(format!("the Console role didn't move: {error}"));
            lines.extend(self.drop_record(store));
            return Err(lines);
        }
        if let Err(error) = moved(endpoints, &record.silent, Role::Multimedia) {
            let mut lines = not_moved(format!("the Multimedia role didn't move: {error}"));
            match moved(endpoints, &record.console, Role::Console) {
                Ok(()) => lines.extend(self.drop_record(store)),
                Err(error) => lines.push(format!(
                    "default output: the Console role couldn't be put back, the record stays \
                     for the next launch: {error}"
                )),
            }
            return Err(lines);
        }
        let line = format!(
            "default output: moved to the silent endpoint {} for Console and Multimedia (was {} \
             and {})",
            record.silent, record.console, record.multimedia
        );
        self.state = State::Switched(record);
        Ok(line)
    }

    fn drop_record(&mut self, store: &mut dyn RecordStore) -> Option<String> {
        self.save(store, None)
            .err()
            .map(|error| format!("default output: the restore record couldn't be deleted: {error}"))
    }

    /// While switched and streaming: a silent endpoint that went away or
    /// that the user chose as an output, and a default the user moved, end
    /// the switch for the rest of the stream.
    pub fn check(
        &mut self,
        endpoints: &mut dyn DefaultEndpoints,
        store: &mut dyn RecordStore,
        chosen: &[&str],
    ) -> Vec<String> {
        let State::Switched(record) = &self.state else {
            return Vec::new();
        };
        let record = record.clone();
        let Ok(listed) = endpoints.endpoints() else {
            return Vec::new();
        };
        let (why, every_role) = if !listed.iter().any(|candidate| candidate.id == record.silent) {
            ("the silent endpoint is gone", true)
        } else if chosen.contains(&record.silent.as_str()) {
            ("the silent endpoint was chosen as an output", true)
        } else if ROLES.iter().any(|&role| {
            endpoints
                .default_id(role)
                .is_ok_and(|id| id.is_some_and(|id| id != record.silent))
        }) {
            ("the default output was changed outside AudioSync", false)
        } else {
            return Vec::new();
        };
        self.state = State::Fallback;
        let mut lines = vec![format!("default output: {why}")];
        lines.extend(self.put_back(endpoints, store, &record, every_role));
        lines.push(
            "default output: Windows' default output gets no copy while this stream runs"
                .to_owned(),
        );
        lines
    }

    /// Ends the switch: at a stop, a source change or a quit, or with
    /// `streaming` when the session is ending while the stream runs on.
    pub fn restore(
        &mut self,
        endpoints: &mut dyn DefaultEndpoints,
        store: &mut dyn RecordStore,
        streaming: bool,
    ) -> Vec<String> {
        let state = std::mem::take(&mut self.state);
        if streaming {
            self.state = State::Fallback;
        }
        match state {
            State::Switched(record) => self.put_back(endpoints, store, &record, false),
            _ => Vec::new(),
        }
    }

    /// Sets each role still on the silent endpoint back to its recorded
    /// default (every role with `every_role`), the reference output (else
    /// the smallest real one) standing in for one that is no longer active,
    /// then deletes the record unless a role failed or was left.
    fn put_back(
        &mut self,
        endpoints: &mut dyn DefaultEndpoints,
        store: &mut dyn RecordStore,
        record: &Record,
        every_role: bool,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        let listed = endpoints.endpoints().ok();
        let smallest = |kept: &dyn Fn(OutputKind) -> bool| {
            listed.as_ref().and_then(|listed| {
                listed
                    .iter()
                    .filter(|candidate| kept(candidate.kind))
                    .map(|candidate| candidate.id.clone())
                    .min()
            })
        };
        let reference = smallest(&|kind| kind == OutputKind::BuiltIn)
            .or_else(|| smallest(&|kind| kind != OutputKind::Virtual));
        let mut failed = false;
        for role in ROLES {
            if !every_role {
                match endpoints.default_id(role) {
                    Ok(Some(id)) if id == record.silent => {}
                    Ok(_) => {
                        lines.push(format!(
                            "default output: {role:?} left as it is, it isn't on the silent \
                             endpoint"
                        ));
                        continue;
                    }
                    Err(error) => {
                        failed = true;
                        lines.push(format!(
                            "default output: the {role:?} default can't be read: {error}"
                        ));
                        continue;
                    }
                }
            }
            let previous = record.previous(role);
            let active = listed
                .as_ref()
                .is_none_or(|listed| listed.iter().any(|candidate| candidate.id == previous));
            let target = if active {
                previous.to_owned()
            } else if let Some(reference) = &reference {
                reference.clone()
            } else {
                failed = true;
                lines.push(format!(
                    "default output: {role:?} left as it is, {previous} isn't active and no \
                     other real output is"
                ));
                continue;
            };
            match moved(endpoints, &target, role) {
                Ok(()) => lines.push(format!("default output: {role:?} restored to {target}")),
                Err(error) => {
                    failed = true;
                    lines.push(format!(
                        "default output: {role:?} couldn't be restored to {target}: {error}"
                    ));
                }
            }
        }
        if failed {
            lines.push("default output: the restore record stays for the next launch".to_owned());
        } else {
            lines.extend(self.drop_record(store));
        }
        lines
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::audio::test_cells::SyncRefCell;
    use std::sync::Arc;

    pub(crate) const SPEAKERS: &str = "{0.0.0.00000000}.speakers";
    const HEADSET: &str = "{0.0.0.00000000}.headset";
    pub(crate) const CABLE: &str = "{0.0.0.00000000}.cable";
    const MIXER: &str = "{0.0.0.00000000}.mixer";
    const PHONE: &str = "{0.0.0.00000000}.phone";

    /// What happened, in order, across the fake endpoints and store.
    pub(crate) type Calls = Arc<SyncRefCell<Vec<String>>>;

    pub(crate) struct Fake {
        calls: Calls,
        pub(crate) endpoints: Vec<Candidate>,
        pub(crate) console: Option<String>,
        pub(crate) multimedia: Option<String>,
        /// The communications default, which nothing may change.
        communications: String,
        refuse: Option<Role>,
        /// Set calls that report success without moving the default.
        ignored: bool,
        pub(crate) broken: bool,
    }

    impl Fake {
        pub(crate) fn new(calls: &Calls) -> Fake {
            Fake {
                calls: Arc::clone(calls),
                endpoints: vec![
                    candidate(SPEAKERS, OutputKind::BuiltIn),
                    candidate(HEADSET, OutputKind::Usb),
                    candidate(CABLE, OutputKind::Virtual),
                ],
                console: Some(SPEAKERS.to_owned()),
                multimedia: Some(SPEAKERS.to_owned()),
                communications: HEADSET.to_owned(),
                refuse: None,
                ignored: false,
                broken: false,
            }
        }

        fn slot(&mut self, role: Role) -> &mut Option<String> {
            match role {
                Role::Console => &mut self.console,
                Role::Multimedia => &mut self.multimedia,
            }
        }
    }

    impl DefaultEndpoints for Fake {
        fn default_id(&mut self, role: Role) -> Result<Option<String>, String> {
            if self.broken {
                return Err("class not registered".to_owned());
            }
            Ok(self.slot(role).clone())
        }

        fn set_default(&mut self, id: &str, role: Role) -> Result<(), String> {
            self.calls.borrow_mut().push(format!("set {role:?} {id}"));
            if self.broken || self.refuse == Some(role) {
                return Err("E_FAIL".to_owned());
            }
            if !self.ignored {
                *self.slot(role) = Some(id.to_owned());
            }
            Ok(())
        }

        fn endpoints(&mut self) -> Result<Vec<Candidate>, String> {
            Ok(self.endpoints.clone())
        }
    }

    pub(crate) struct Store {
        calls: Calls,
        pub(crate) record: Option<Record>,
        fails: bool,
    }

    impl Store {
        pub(crate) fn new(calls: &Calls) -> Store {
            Store {
                calls: Arc::clone(calls),
                record: None,
                fails: false,
            }
        }
    }

    impl RecordStore for Store {
        fn load(&mut self) -> Result<Option<Record>, String> {
            Ok(self.record.clone())
        }

        fn save(&mut self, record: Option<&Record>) -> Result<(), String> {
            let what = if record.is_some() { "write" } else { "delete" };
            self.calls.borrow_mut().push(format!("{what} record"));
            if self.fails {
                return Err("access denied".to_owned());
            }
            self.record = record.cloned();
            Ok(())
        }
    }

    /// A fake shared with the test once the controller holds it.
    pub(crate) struct Shared<T>(pub(crate) Arc<SyncRefCell<T>>);

    impl<T> Clone for Shared<T> {
        fn clone(&self) -> Self {
            Shared(Arc::clone(&self.0))
        }
    }

    impl<T: DefaultEndpoints> DefaultEndpoints for Shared<T> {
        fn default_id(&mut self, role: Role) -> Result<Option<String>, String> {
            self.0.borrow_mut().default_id(role)
        }

        fn set_default(&mut self, id: &str, role: Role) -> Result<(), String> {
            self.0.borrow_mut().set_default(id, role)
        }

        fn endpoints(&mut self) -> Result<Vec<Candidate>, String> {
            self.0.borrow_mut().endpoints()
        }
    }

    impl<T: RecordStore> RecordStore for Shared<T> {
        fn load(&mut self) -> Result<Option<Record>, String> {
            self.0.borrow_mut().load()
        }

        fn save(&mut self, record: Option<&Record>) -> Result<(), String> {
            self.0.borrow_mut().save(record)
        }
    }

    fn candidate(id: &str, kind: OutputKind) -> Candidate {
        Candidate {
            id: id.to_owned(),
            kind,
            listened: false,
            communications: false,
        }
    }

    fn world() -> (Calls, Fake, Store) {
        let calls = Calls::default();
        let fake = Fake::new(&calls);
        let store = Store::new(&calls);
        (calls, fake, store)
    }

    fn switched(fake: &mut Fake, store: &mut Store) -> DefaultSwitch {
        let mut switch = DefaultSwitch::default();
        switch.engage(fake, store, &[SPEAKERS]);
        assert!(switch.switched(), "the switch is in effect");
        switch
    }

    fn defaults(fake: &Fake) -> (Option<&str>, Option<&str>) {
        (fake.console.as_deref(), fake.multimedia.as_deref())
    }

    pub(crate) fn the_record() -> Record {
        Record {
            console: SPEAKERS.to_owned(),
            multimedia: SPEAKERS.to_owned(),
            silent: CABLE.to_owned(),
        }
    }

    // covers: S469
    #[test]
    fn the_record_is_written_before_the_first_switch() {
        let (calls, mut fake, mut store) = world();
        let switch = switched(&mut fake, &mut store);
        assert_eq!(
            *calls.borrow(),
            [
                "write record".to_owned(),
                format!("set Console {CABLE}"),
                format!("set Multimedia {CABLE}"),
            ]
        );
        assert_eq!(store.record, Some(the_record()));
        assert_eq!(switch.record(), Some(&the_record()));
        assert_eq!(defaults(&fake), (Some(CABLE), Some(CABLE)));
        assert!(!switch.holds_out_default());
    }

    // covers: S469, S470
    #[test]
    fn a_record_that_cant_be_written_switches_nothing() {
        let (calls, mut fake, mut store) = world();
        store.fails = true;
        let mut switch = DefaultSwitch::default();
        switch.engage(&mut fake, &mut store, &[SPEAKERS]);
        assert_eq!(*calls.borrow(), ["write record".to_owned()]);
        assert_eq!(defaults(&fake), (Some(SPEAKERS), Some(SPEAKERS)));
        assert!(switch.holds_out_default());
    }

    // covers: S469
    #[test]
    fn the_communications_role_is_never_touched() {
        let (calls, mut fake, mut store) = world();
        let mut switch = switched(&mut fake, &mut store);
        switch.restore(&mut fake, &mut store, false);
        assert_eq!(fake.communications, HEADSET);
        assert!(
            calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with("set"))
                .all(|call| call.contains("Console") || call.contains("Multimedia"))
        );
    }

    // covers: S469
    #[test]
    fn a_stop_or_quit_restores_both_roles_and_deletes_the_record() {
        let (_, mut fake, mut store) = world();
        let mut switch = switched(&mut fake, &mut store);
        switch.restore(&mut fake, &mut store, false);
        assert_eq!(defaults(&fake), (Some(SPEAKERS), Some(SPEAKERS)));
        assert_eq!(store.record, None);
        assert_eq!(switch.record(), None);
        assert!(!switch.engaged());
    }

    // covers: S469
    #[test]
    fn the_next_launch_restores_a_record_left_on_disk() {
        let (_, mut fake, mut store) = world();
        fake.console = Some(CABLE.to_owned());
        fake.multimedia = Some(CABLE.to_owned());
        store.record = Some(the_record());
        let mut switch = DefaultSwitch::default();
        switch.restore_at_launch(&mut fake, &mut store);
        assert_eq!(defaults(&fake), (Some(SPEAKERS), Some(SPEAKERS)));
        assert_eq!(store.record, None);
    }

    // covers: S469
    #[test]
    fn a_role_that_fails_to_restore_keeps_the_record() {
        let (_, mut fake, mut store) = world();
        let mut switch = switched(&mut fake, &mut store);
        fake.refuse = Some(Role::Multimedia);
        switch.restore(&mut fake, &mut store, false);
        assert_eq!(defaults(&fake), (Some(SPEAKERS), Some(CABLE)));
        assert_eq!(store.record, Some(the_record()));
        assert_eq!(switch.record(), Some(&the_record()));
    }

    // covers: S469
    #[test]
    fn a_record_kept_after_a_failed_restore_is_never_overwritten_with_the_silent_endpoint() {
        let (_, mut fake, mut store) = world();
        let mut switch = switched(&mut fake, &mut store);
        fake.refuse = Some(Role::Multimedia);
        switch.restore(&mut fake, &mut store, false);
        assert_eq!(defaults(&fake), (Some(SPEAKERS), Some(CABLE)));
        fake.refuse = None;
        switch.engage(&mut fake, &mut store, &[SPEAKERS]);
        assert_eq!(store.record, Some(the_record()));
        switch.restore(&mut fake, &mut store, false);
        assert_eq!(defaults(&fake), (Some(SPEAKERS), Some(SPEAKERS)));
        assert_eq!(store.record, None);
    }

    // covers: S469
    #[test]
    fn without_a_reference_a_gone_default_restores_to_another_real_endpoint() {
        let (_, mut fake, mut store) = world();
        fake.console = Some(HEADSET.to_owned());
        fake.multimedia = Some(HEADSET.to_owned());
        let mut switch = switched(&mut fake, &mut store);
        fake.endpoints = vec![
            candidate(CABLE, OutputKind::Virtual),
            candidate(PHONE, OutputKind::Bluetooth),
        ];
        switch.restore(&mut fake, &mut store, false);
        assert_eq!(defaults(&fake), (Some(PHONE), Some(PHONE)));
        assert_eq!(store.record, None);
    }

    // covers: S469
    #[test]
    fn with_no_real_endpoint_left_the_record_stays() {
        let (_, mut fake, mut store) = world();
        let mut switch = switched(&mut fake, &mut store);
        fake.endpoints = vec![candidate(CABLE, OutputKind::Virtual)];
        switch.restore(&mut fake, &mut store, false);
        assert_eq!(store.record, Some(the_record()));
    }

    // covers: S469
    #[test]
    fn a_previous_default_no_longer_active_restores_to_the_reference() {
        let (_, mut fake, mut store) = world();
        fake.console = Some(HEADSET.to_owned());
        fake.multimedia = Some(HEADSET.to_owned());
        let mut switch = switched(&mut fake, &mut store);
        fake.endpoints.retain(|candidate| candidate.id != HEADSET);
        switch.restore(&mut fake, &mut store, false);
        assert_eq!(defaults(&fake), (Some(SPEAKERS), Some(SPEAKERS)));
    }

    // covers: S469, S470
    #[test]
    fn the_users_own_change_wins() {
        let (calls, mut fake, mut store) = world();
        let mut switch = switched(&mut fake, &mut store);
        fake.multimedia = Some(HEADSET.to_owned());
        calls.borrow_mut().clear();
        switch.check(&mut fake, &mut store, &[SPEAKERS]);
        assert_eq!(defaults(&fake), (Some(SPEAKERS), Some(HEADSET)));
        assert_eq!(
            *calls.borrow(),
            [
                format!("set Console {SPEAKERS}"),
                "delete record".to_owned()
            ]
        );
        assert!(switch.holds_out_default());
        // Not switched back, nor restored over at the stop.
        switch.check(&mut fake, &mut store, &[SPEAKERS]);
        switch.restore(&mut fake, &mut store, false);
        assert_eq!(defaults(&fake), (Some(SPEAKERS), Some(HEADSET)));
    }

    // covers: S469, S470
    #[test]
    fn a_vanished_silent_endpoint_restores_both_roles() {
        let (_, mut fake, mut store) = world();
        let mut switch = switched(&mut fake, &mut store);
        fake.endpoints.retain(|candidate| candidate.id != CABLE);
        // Windows picked a default of its own in between.
        fake.console = Some(HEADSET.to_owned());
        fake.multimedia = Some(HEADSET.to_owned());
        switch.check(&mut fake, &mut store, &[SPEAKERS]);
        assert_eq!(defaults(&fake), (Some(SPEAKERS), Some(SPEAKERS)));
        assert_eq!(store.record, None);
        assert!(switch.holds_out_default());
    }

    // covers: S469, S470
    #[test]
    fn choosing_the_silent_endpoint_as_an_output_ends_the_switch() {
        let (_, mut fake, mut store) = world();
        let mut switch = switched(&mut fake, &mut store);
        switch.check(&mut fake, &mut store, &[SPEAKERS, CABLE]);
        assert_eq!(defaults(&fake), (Some(SPEAKERS), Some(SPEAKERS)));
        assert!(switch.holds_out_default());
    }

    // covers: S469, S470
    #[test]
    fn an_interface_that_fails_falls_back_without_a_switch() {
        let (_, mut fake, mut store) = world();
        fake.broken = true;
        let mut switch = DefaultSwitch::default();
        switch.engage(&mut fake, &mut store, &[SPEAKERS]);
        assert!(switch.holds_out_default());
        assert_eq!(store.record, None);
    }

    // covers: S469, S470
    #[test]
    fn a_success_that_doesnt_move_the_default_falls_back() {
        let (_, mut fake, mut store) = world();
        fake.ignored = true;
        let mut switch = DefaultSwitch::default();
        switch.engage(&mut fake, &mut store, &[SPEAKERS]);
        assert!(switch.holds_out_default());
        assert_eq!(store.record, None);
        assert_eq!(defaults(&fake), (Some(SPEAKERS), Some(SPEAKERS)));
    }

    // covers: S469
    #[test]
    fn a_second_role_that_fails_puts_the_first_back() {
        let (calls, mut fake, mut store) = world();
        fake.refuse = Some(Role::Multimedia);
        let mut switch = DefaultSwitch::default();
        switch.engage(&mut fake, &mut store, &[SPEAKERS]);
        assert_eq!(defaults(&fake), (Some(SPEAKERS), Some(SPEAKERS)));
        assert_eq!(
            calls.borrow().last().map(String::as_str),
            Some("delete record")
        );
        assert!(switch.holds_out_default());
    }

    // covers: S469, S470
    #[test]
    fn without_a_silent_endpoint_nothing_moves() {
        let (calls, mut fake, mut store) = world();
        let mut switch = DefaultSwitch::default();
        // The only virtual endpoint is an output the user chose.
        switch.engage(&mut fake, &mut store, &[SPEAKERS, CABLE]);
        assert!(calls.borrow().is_empty());
        assert!(switch.holds_out_default());
    }

    // covers: S469
    #[test]
    fn a_default_already_silent_is_neither_moved_nor_recorded() {
        let (calls, mut fake, mut store) = world();
        fake.console = Some(CABLE.to_owned());
        fake.multimedia = Some(CABLE.to_owned());
        let mut switch = DefaultSwitch::default();
        switch.engage(&mut fake, &mut store, &[SPEAKERS]);
        assert!(calls.borrow().is_empty());
        assert!(!switch.holds_out_default());
        assert!(!switch.switched());
    }

    // covers: S469
    #[test]
    fn the_silent_endpoint_is_virtual_unlistened_first_then_the_smallest_id() {
        let mut listed = vec![
            candidate(SPEAKERS, OutputKind::BuiltIn),
            candidate(MIXER, OutputKind::Virtual),
            candidate(CABLE, OutputKind::Virtual),
            candidate(PHONE, OutputKind::Bluetooth),
        ];
        assert_eq!(silent_endpoint(&listed, &[]), Some(CABLE));
        listed[2].listened = true;
        assert_eq!(silent_endpoint(&listed, &[]), Some(MIXER));
        assert_eq!(silent_endpoint(&listed, &[MIXER]), Some(CABLE));
        assert_eq!(silent_endpoint(&listed[..1], &[]), None);
    }

    // covers: S486
    #[test]
    fn the_communications_endpoint_is_never_the_silent_one() {
        let mut listed = vec![
            candidate(MIXER, OutputKind::Virtual),
            candidate(CABLE, OutputKind::Virtual),
        ];
        listed[1].communications = true;
        assert_eq!(silent_endpoint(&listed, &[]), Some(MIXER));
        listed[0].communications = true;
        assert_eq!(silent_endpoint(&listed, &[]), None);
    }
}
