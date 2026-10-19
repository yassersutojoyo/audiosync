//! The UI's [`Backend`] over the audio backend.
//!
//! The windows show what the audio backend lists, the sources to capture, the
//! connected outputs and the AirPlay speakers with their settings, and every
//! user action becomes audio backend calls, decided by the session rules of
//! `audiosync-core` (`policy::session`): switching a speaker on starts
//! streaming, switching the last one off stops it, an output that can't start
//! is turned off, a source that quits ends the session. AirPlay speakers count
//! like any other speaker; one that can't play from AudioSync (a password,
//! AirPlay 2 only) is listed with that note and stays off, with a notice when
//! the user tries it. An Apple TV that asks for a PIN is paired when the user
//! switches it on (never by auto-connect, which only posts a notice): the
//! snapshot's [`PinPrompt`] asks the UI for the PIN, and once paired the
//! speaker is on and streaming starts like for any speaker switched on. What
//! happens on its own comes from
//! [`AudioBackend::poll`], which [`Backend::poll`] passes on, and the adapter
//! turns it into notices. Notices carry a [`NoticeMessage`]; the UI words it.
//!
//! The chosen source, groups, auto-connect marks and notices live here, not in
//! the audio backend. The double-audio notice (the source heard straight from
//! an output as well as through AudioSync) follows what the backend reports:
//! it is posted when the outputs it names change, changed in place when only
//! which of them AudioSync plays to changes, and cleared when the cause is
//! gone. Once dismissed, it stays away for the same source and those outputs
//! (or fewer of them) until that app quits.
//!
//! The user's choices outlive a restart (`crate::settings`): the adapter
//! reads the settings file before the first listing and hands the saved
//! switches, volumes and delays to the audio backend, which keeps them by ID
//! for a device that isn't there yet; at each poll it writes the file again
//! if anything changed. Like the Mac app, a launch doesn't start streaming
//! by itself, except through an auto-connect speaker that is there. Saved
//! settings of a device that is gone stay in the file for when it is back.
//!
//! Speakers are named by the core's speaker keys: `lc:` and the endpoint ID
//! for a local output, `ap:` and the speaker's ID for an AirPlay speaker, so
//! groups and auto-connect marks hold both. Sources are `system`, or `app:` with the app's process ID
//! and executable (`app:2000:spotify.exe`), so a process ID that Windows gives
//! to another app later is never taken for the chosen one.
//!
//! The debug log gets what the user asked for, what the session rules
//! decided, the notices posted and the source list whenever it changes; the
//! audio backend logs what it did itself.

use std::collections::{BTreeMap, BTreeSet};

use audiosync_core::eq::EqSettings;
use audiosync_core::policy::delay::{native_airplay_adjustment_ms, native_airplay_latency_ms};
use audiosync_core::policy::pairing::DropReason;
use audiosync_core::policy::session::{
    AutoStartGate, Incident, SpeakerEvent, StartOrigin, StreamingAction, may_start, reaction,
    source_switch, streaming_after,
};
use audiosync_core::policy::speakers::{NoticeSubject, Notices, SpeakerKey, renamed_group};

use crate::audio::backend::{
    AirPlayAvailability, AirPlayInfo, AirPlayProblem, AudioBackend, BackendError, BackendEvent,
    OutputInfo, OutputKind, SourceId, SourceInfo, StopReason,
};
use crate::audio::backend::{AirPlayPinPrompt, PairingPhase};
use crate::audio::capture::WHOLE_SYSTEM_NAME;
use crate::audio::routing::{DirectPath, DirectReason};
use crate::debug_log::DebugLog;
use crate::settings::{
    self, SavedEqualizer, SavedGroup, SavedSource, SavedSpeaker, Settings, SettingsFile,
};
use crate::ui_model::{
    self, AppState, Availability, Backend, Group, GroupError, Notice, NoticeMessage, PinPhase,
    PinPrompt, Source, SourceKind, Speaker, SpeakerKind, ViewSettings, clamp_delay_ms,
};

/// The source list is read again every this many polls, about every three
/// seconds: it takes a snapshot of the running processes, and apps that play
/// audio come and go rarely.
const SOURCE_POLLS: u32 = 6;

/// The UI's ID of the whole-system source.
const SYSTEM_KEY: &str = "system";

/// The source the next start captures.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Chosen {
    WholeSystem,
    /// An app, kept when it quits and followed by its executable when it
    /// starts again with a new process ID.
    App {
        root_pid: u32,
        executable: String,
        name: String,
    },
}

impl Chosen {
    fn from_source(source: &SourceInfo) -> Chosen {
        match source.id {
            SourceId::WholeSystem => Chosen::WholeSystem,
            SourceId::App { root_pid } => Chosen::App {
                root_pid,
                executable: source.executable.clone(),
                name: source.name.clone(),
            },
        }
    }

    fn id(&self) -> SourceId {
        match self {
            Chosen::WholeSystem => SourceId::WholeSystem,
            Chosen::App { root_pid, .. } => SourceId::App {
                root_pid: *root_pid,
            },
        }
    }

    /// The UI's ID of this source.
    fn key(&self) -> String {
        match self {
            Chosen::WholeSystem => SYSTEM_KEY.to_owned(),
            Chosen::App {
                root_pid,
                executable,
                ..
            } => app_key(*root_pid, executable),
        }
    }
}

/// A notice as it was posted.
#[derive(Clone, Debug)]
struct Posted {
    id: u64,
    message: NoticeMessage,
    time: String,
}

/// What the double-audio notice is about: the source, by its UI ID, and the
/// outputs it names, by endpoint ID and name.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HeardTwiceSubject {
    source: String,
    endpoints: Vec<(String, String)>,
}

/// The UI's backend over an audio backend. See the module documentation.
pub struct AudioAdapter<A: AudioBackend> {
    audio: A,
    /// The sources as last listed, the whole system first.
    sources: Vec<SourceInfo>,
    chosen: Chosen,
    /// The endpoint IDs of the last listing, to tell which outputs appeared.
    listed: BTreeSet<String>,
    /// The last name of every output listed, for a notice about one that is
    /// gone.
    names: BTreeMap<String, String>,
    /// The AirPlay speaker IDs of the last listing, to tell which appeared.
    airplay_listed: BTreeSet<String>,
    /// The last name of every AirPlay speaker listed.
    airplay_names: BTreeMap<String, String>,
    /// The speaker keys marked auto-connect.
    auto_connect: BTreeSet<String>,
    groups: Vec<Group>,
    next_group: u64,
    gate: AutoStartGate,
    notices: Notices<NoticeSubject, Posted>,
    next_notice: u64,
    /// What the double-audio notice showing is about.
    heard_twice: Option<HeardTwiceSubject>,
    /// The outputs named in a double-audio notice the user dismissed, by
    /// source UI ID, until that app quits.
    heard_twice_dismissed: BTreeMap<String, BTreeSet<String>>,
    equalizer: EqSettings,
    /// Polls since the sources were last listed.
    polls: u32,
    /// The local time now, as a notice shows it.
    clock: fn() -> String,
    /// The debug log; it writes nothing unless `AUDIOSYNC_DEBUG=1`.
    log: DebugLog,
    /// Where the settings are kept.
    settings_file: SettingsFile,
    /// The settings as last read or written: the file holds them.
    saved: Settings,
    /// The settings file is of a format older than the local delay model:
    /// its delays are migrated once an output list is in (spec S180), and
    /// nothing is saved before.
    delays_to_migrate: bool,
    /// Save the settings at the next chance even if they look unchanged:
    /// the file is in an older format.
    must_save: bool,
    /// AirPlay speakers saved as on that the backend couldn't switch on at
    /// launch (no AirPlay sender): their saved settings stay as they are.
    airplay_unrestored: BTreeSet<String>,
    /// The windows' own settings.
    view: ViewSettings,
}

impl<A: AudioBackend> AudioAdapter<A> {
    /// The adapter over `audio`, stopped, with the sources and outputs listed
    /// now and nothing saved. `clock` gives the time a notice shows; what
    /// happens goes to `log`.
    #[cfg(test)]
    pub fn new(audio: A, clock: fn() -> String, log: DebugLog) -> AudioAdapter<A> {
        AudioAdapter::with_settings(audio, clock, log, SettingsFile::in_memory())
    }

    /// The adapter over `audio` with the settings in `settings_file`,
    /// stopped unless an auto-connect speaker is there, with the sources and
    /// outputs listed now. `clock` gives the time a notice shows; what
    /// happens goes to `log`.
    pub fn with_settings(
        audio: A,
        clock: fn() -> String,
        log: DebugLog,
        settings_file: SettingsFile,
    ) -> AudioAdapter<A> {
        let (saved, loaded) = settings_file.load();
        if settings_file.path().is_some() {
            log.log(|| format!("settings: {loaded}"));
        } else {
            log.log(|| "settings: no app data folder; kept until AudioSync quits".to_owned());
        }
        let mut adapter = AudioAdapter {
            audio,
            sources: vec![SourceInfo {
                id: SourceId::WholeSystem,
                name: WHOLE_SYSTEM_NAME.to_owned(),
                executable: String::new(),
            }],
            chosen: Chosen::WholeSystem,
            listed: BTreeSet::new(),
            names: BTreeMap::new(),
            airplay_listed: BTreeSet::new(),
            airplay_names: BTreeMap::new(),
            auto_connect: BTreeSet::new(),
            groups: Vec::new(),
            next_group: 1,
            gate: AutoStartGate::default(),
            notices: Notices::new(),
            next_notice: 1,
            heard_twice: None,
            heard_twice_dismissed: BTreeMap::new(),
            equalizer: EqSettings::default(),
            polls: 0,
            clock,
            log,
            settings_file,
            saved: Settings::default(),
            delays_to_migrate: matches!(
                loaded,
                settings::Loaded::Read { version, .. } if version < settings::OFFSETS_VERSION
            ),
            must_save: false,
            airplay_unrestored: BTreeSet::new(),
            view: ViewSettings::default(),
        };
        adapter.restore(saved);
        adapter.list_devices();
        adapter.log.log(|| match &adapter.chosen {
            Chosen::WholeSystem => format!("source chosen: {WHOLE_SYSTEM_NAME} (the default)"),
            Chosen::App { name, .. } => {
                format!("source chosen: {name} ({}), as saved", adapter.chosen.key())
            }
        });
        adapter
    }

    /// Takes in the saved settings before anything is listed: the audio
    /// backend keeps each speaker's by ID, so one that isn't there yet gets
    /// them when it appears. Nothing starts streaming here; auto-connect
    /// speakers do once the first listing finds them.
    fn restore(&mut self, saved: Settings) {
        self.audio.set_master_volume(saved.master_volume);
        if !self.delays_to_migrate {
            self.audio
                .set_airplay_adjustment(saved.airplay_adjustment_ms);
        }
        for (id, speaker) in &saved.outputs {
            // An older file's trims become offsets once the reference is
            // known ([`AudioAdapter::migrate_delays_if_due`]).
            if !self.delays_to_migrate {
                self.audio.set_output_delay(id, speaker.delay_ms);
            }
            self.audio.set_output_volume(id, speaker.volume);
            if speaker.enabled {
                // Nothing streams yet, so this only marks it on.
                let _ = self.audio.set_output_enabled(id, true);
            }
        }
        for (id, speaker) in &saved.airplay {
            self.audio.set_airplay_volume(id, speaker.volume);
            self.audio.set_airplay_delay(id, speaker.delay_ms);
            if speaker.enabled && self.audio.set_airplay_enabled(id, true).is_err() {
                self.airplay_unrestored.insert(id.clone());
            }
        }
        if !self.airplay_unrestored.is_empty() {
            let count = self.airplay_unrestored.len();
            self.log.log(|| {
                format!("settings: {count} AirPlay speaker(s) stay off without an AirPlay sender")
            });
        }
        self.auto_connect = saved.auto_connect.clone();
        self.groups = saved
            .groups
            .iter()
            .map(|group| Group {
                id: group.id.clone(),
                name: group.name.clone(),
                members: group.members.clone(),
            })
            .collect();
        self.next_group = self
            .groups
            .iter()
            .filter_map(|group| group.id.strip_prefix("group:")?.parse::<u64>().ok())
            .max()
            .map_or(1, |last| last.saturating_add(1));
        self.equalizer = saved.equalizer.settings();
        self.audio.set_equalizer(self.equalizer);
        self.chosen = match &saved.source {
            SavedSource::System => Chosen::WholeSystem,
            SavedSource::App { executable, name } => Chosen::App {
                // No process yet: the first listing finds the app if it
                // runs, by its executable.
                root_pid: 0,
                executable: executable.clone(),
                name: if name.is_empty() {
                    executable.clone()
                } else {
                    name.clone()
                },
            },
        };
        self.view = ViewSettings {
            show_virtual_devices: saved.show_virtual_devices,
            window: saved.window,
        };
        self.saved = saved;
    }

    /// The speakers' settings to save: the saved ones, updated with what the
    /// backend holds for the speakers listed now; `enabled` from `on` for
    /// every speaker except the `held` ones. Defaults aren't kept.
    fn merged_speakers(
        saved: &BTreeMap<String, SavedSpeaker>,
        listed: impl IntoIterator<Item = (String, SavedSpeaker)>,
        on: &[String],
        held: &BTreeSet<String>,
    ) -> BTreeMap<String, SavedSpeaker> {
        let mut speakers = saved.clone();
        for (id, speaker) in speakers.iter_mut() {
            if !held.contains(id) {
                speaker.enabled = on.contains(id);
            }
        }
        speakers.extend(listed);
        for id in on {
            speakers.entry(id.clone()).or_insert(SavedSpeaker {
                enabled: true,
                ..SavedSpeaker::default()
            });
        }
        speakers.retain(|_, speaker| !speaker.is_default());
        speakers
    }

    /// The settings as they are now, to compare with the file.
    fn current_settings(&self) -> Settings {
        let outputs = Self::merged_speakers(
            &self.saved.outputs,
            self.audio.outputs().into_iter().map(|output| {
                let speaker = SavedSpeaker {
                    enabled: output.enabled,
                    volume: output.volume,
                    delay_ms: output.delay_ms,
                };
                (output.id, speaker)
            }),
            &self.audio.enabled_outputs(),
            &BTreeSet::new(),
        );
        let airplay = Self::merged_speakers(
            &self.saved.airplay,
            self.audio.airplay_speakers().into_iter().map(|speaker| {
                let saved = SavedSpeaker {
                    enabled: speaker.enabled,
                    volume: speaker.volume,
                    delay_ms: speaker.delay_ms,
                };
                (speaker.id, saved)
            }),
            &self.audio.enabled_airplay(),
            &self.airplay_unrestored,
        );
        Settings {
            version: settings::VERSION,
            source: match &self.chosen {
                Chosen::WholeSystem => SavedSource::System,
                Chosen::App {
                    executable, name, ..
                } => SavedSource::App {
                    executable: executable.clone(),
                    name: name.clone(),
                },
            },
            master_volume: self.audio.master_volume(),
            outputs,
            airplay,
            auto_connect: self.auto_connect.clone(),
            groups: self
                .groups
                .iter()
                .map(|group| SavedGroup {
                    id: group.id.clone(),
                    name: group.name.clone(),
                    members: group.members.clone(),
                })
                .collect(),
            equalizer: SavedEqualizer::from(self.equalizer),
            show_virtual_devices: self.view.show_virtual_devices,
            window: self.view.window,
            airplay_adjustment_ms: self.audio.airplay_adjustment(),
        }
        .clamped()
    }

    /// Writes the settings file if anything changed since it was last read
    /// or written. A failed write is logged and tried again at the next
    /// change; the settings stay in effect for this run. Nothing is written
    /// while an older file's delays wait for their migration.
    fn persist(&mut self) {
        if self.delays_to_migrate {
            return;
        }
        let current = self.current_settings();
        if current == self.saved && !self.must_save {
            return;
        }
        self.must_save = false;
        match self.settings_file.save(&current) {
            Ok(kept) if self.settings_file.path().is_some() => {
                if kept.is_some() {
                    self.log.log(|| {
                        format!(
                            "settings: a newer AudioSync's file was kept as {} before saving over it",
                            crate::settings::NEWER_FILE_NAME
                        )
                    });
                }
                self.log.log(|| "settings: saved".to_owned());
            }
            Ok(_) => {}
            Err(error) => self.log.log(|| {
                format!("settings: couldn't be saved ({error}); they last until AudioSync quits")
            }),
        }
        self.saved = current;
    }

    /// The audio backend, to change the mock's pretend world in tests.
    #[cfg(test)]
    pub fn audio_mut(&mut self) -> &mut A {
        &mut self.audio
    }

    fn running(&self) -> bool {
        self.audio.running_source().is_some()
    }

    /// Speakers turned on, local and AirPlay, connected or not.
    fn enabled_count(&self) -> usize {
        self.audio.enabled_outputs().len() + self.audio.enabled_airplay().len()
    }

    /// The streaming policy after `event` (S162), from the speakers on.
    fn streaming_after(&self, event: SpeakerEvent) -> StreamingAction {
        streaming_after(
            event,
            self.running(),
            self.audio.enabled_outputs().len(),
            self.audio.enabled_airplay().len(),
        )
    }

    fn is_airplay_enabled(&self, id: &str) -> bool {
        self.audio.enabled_airplay().iter().any(|on| on == id)
    }

    /// The name of an AirPlay speaker as last listed.
    fn airplay_name(&self, id: &str) -> String {
        self.airplay_names
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_owned())
    }

    fn is_enabled(&self, endpoint_id: &str) -> bool {
        self.audio
            .enabled_outputs()
            .iter()
            .any(|id| id == endpoint_id)
    }

    /// The name of an output as last listed.
    fn output_name(&self, endpoint_id: &str) -> String {
        self.names
            .get(endpoint_id)
            .cloned()
            .unwrap_or_else(|| endpoint_id.to_owned())
    }

    fn chosen_name(&self) -> String {
        match &self.chosen {
            Chosen::WholeSystem => WHOLE_SYSTEM_NAME.to_owned(),
            Chosen::App { name, .. } => name.clone(),
        }
    }

    /// Posts a notice, replacing an older one about the same subject.
    fn post(&mut self, subject: NoticeSubject, message: NoticeMessage) {
        self.log
            .log(|| format!("notice about {subject:?}: {message:?}"));
        let id = self.next_notice;
        self.next_notice += 1;
        let time = (self.clock)();
        self.notices.post(subject, Posted { id, message, time });
    }

    /// Clears the notice about `subject` if `kind` holds for what it says.
    fn clear_if(&mut self, subject: NoticeSubject, kind: fn(&NoticeMessage) -> bool) {
        if self.notices.get(&subject).is_some_and(|p| kind(&p.message)) {
            self.notices.clear(&[subject]);
        }
    }

    /// Lists the outputs and the sources again.
    fn list_devices(&mut self) {
        // A failed listing keeps the last list; the backend tries again at
        // its next poll.
        let listed = self.audio.list_outputs().is_ok();
        self.take_events();
        self.outputs_listed();
        if listed {
            self.migrate_delays_if_due();
        }
        self.airplay_listed();
        self.refresh_sources();
    }

    /// Takes in the outputs as last listed: remembers their names, clears the
    /// notice about a lost output that is back, and switches on the
    /// auto-connect speakers that appeared.
    fn outputs_listed(&mut self) {
        let mut listed = BTreeSet::new();
        let mut appeared = Vec::new();
        for output in self.audio.outputs() {
            if !self.listed.contains(&output.id) {
                appeared.push(output.id.clone());
            }
            self.names.insert(output.id.clone(), output.name);
            listed.insert(output.id);
        }
        self.listed = listed;
        for id in &appeared {
            self.clear_if(speaker_subject(id), |message| {
                matches!(message, NoticeMessage::SpeakerLost { .. })
            });
        }
        self.auto_connect(&appeared);
    }

    /// Takes in the AirPlay speakers as last listed: remembers their names,
    /// and switches on the auto-connect speakers that appeared.
    fn airplay_listed(&mut self) {
        let mut listed = BTreeSet::new();
        let mut appeared = Vec::new();
        for speaker in self.audio.airplay_speakers() {
            if !self.airplay_listed.contains(&speaker.id) {
                appeared.push(speaker.id.clone());
            }
            self.airplay_names.insert(speaker.id.clone(), speaker.name);
            listed.insert(speaker.id);
        }
        self.airplay_listed = listed;
        let hits: Vec<String> = appeared
            .into_iter()
            .filter(|id| self.auto_connect.contains(&airplay_key(id)))
            .collect();
        if hits.is_empty() {
            return;
        }
        self.log.log(|| {
            let names: Vec<String> = hits.iter().map(|id| self.airplay_name(id)).collect();
            format!("auto-connect: switching on {}", names.join(", "))
        });
        for id in &hits {
            if !self.is_airplay_enabled(id)
                && let Err(problem) = self.audio.set_airplay_enabled(id, true)
            {
                self.airplay_cant_play(id, problem);
            }
        }
        if !self.running() && self.enabled_count() > 0 {
            self.start_session(StartOrigin::AutoConnect);
        }
    }

    /// Switches on the auto-connect speakers among `appeared`, then starts
    /// streaming once for all of them, unless the user switched streaming off
    /// or an automatic start failed (S120).
    fn auto_connect(&mut self, appeared: &[String]) {
        let hits: Vec<&String> = appeared
            .iter()
            .filter(|id| self.auto_connect.contains(&speaker_key(id)))
            .collect();
        if hits.is_empty() {
            return;
        }
        self.log.log(|| {
            let names: Vec<String> = hits.iter().map(|id| self.output_name(id)).collect();
            format!("auto-connect: switching on {}", names.join(", "))
        });
        for id in hits {
            if !self.is_enabled(id) && self.audio.set_output_enabled(id, true).is_err() {
                self.output_failed(id, false);
            }
        }
        if !self.running() {
            self.start_session(StartOrigin::AutoConnect);
        }
    }

    /// Lists the sources again, and follows the chosen app to its new process
    /// if it started again. Returns whether the list or the choice changed. A
    /// failed listing keeps the last list.
    fn refresh_sources(&mut self) -> bool {
        let Ok(sources) = self.audio.list_sources() else {
            return false;
        };
        let mut chosen = self.chosen.clone();
        if let Chosen::App {
            root_pid,
            executable,
            name,
        } = &mut chosen
        {
            let same_app = |source: &&SourceInfo| source.executable == *executable;
            let found = sources
                .iter()
                .filter(same_app)
                .find(|source| {
                    source.id
                        == SourceId::App {
                            root_pid: *root_pid,
                        }
                })
                .or_else(|| sources.iter().find(same_app));
            if let Some(SourceInfo {
                id: SourceId::App { root_pid: pid },
                name: listed_name,
                ..
            }) = found
            {
                *root_pid = *pid;
                *name = listed_name.clone();
            }
        }
        if sources != self.sources {
            self.log.log(|| {
                let listed: Vec<String> = sources.iter().map(describe_source).collect();
                format!("sources listed: {}", listed.join("; "))
            });
        }
        if chosen != self.chosen {
            self.log
                .log(|| format!("the chosen app now runs as {}", chosen.key()));
        }
        let changed = sources != self.sources || chosen != self.chosen;
        self.sources = sources;
        self.chosen = chosen;
        changed
    }

    /// The source to start: the whole system, or the chosen app where it runs
    /// now. The app needn't play yet: capture hears it once it does.
    fn chosen_source(&mut self) -> Result<SourceId, BackendError> {
        if self.chosen == Chosen::WholeSystem {
            return Ok(SourceId::WholeSystem);
        }
        self.refresh_sources();
        let key = self.chosen.key();
        if let Chosen::App {
            root_pid,
            executable,
            ..
        } = &self.chosen
            && !self.sources.iter().any(|source| source_key(source) == key)
        {
            // Not listed: it has no audio session right now, or it quit and
            // its process ID may belong to another program by now. The
            // process list tells which.
            let app = self
                .audio
                .find_app(*root_pid, executable)?
                .ok_or(BackendError::SourceGone)?;
            self.chosen = Chosen::from_source(&app);
            self.log.log(|| {
                format!(
                    "the chosen app runs as {}, without an audio stream yet",
                    self.chosen.key()
                )
            });
        }
        Ok(self.chosen.id())
    }

    /// Starts streaming the chosen source to the enabled outputs (S116, S119,
    /// S120). A user's start clears the notices about the last attempt.
    /// Returns whether streaming started.
    fn start_session(&mut self, origin: StartOrigin) -> bool {
        if origin == StartOrigin::User {
            self.gate.user_acted();
            self.notices.clear(&[
                NoticeSubject::Source,
                NoticeSubject::Start,
                NoticeSubject::Capture,
            ]);
        } else if !self.gate.allows(origin) {
            self.log.log(|| {
                format!(
                    "start ({origin:?}) held back: streaming was switched off, \
                     or an automatic start failed"
                )
            });
            return false;
        }
        if !may_start(self.enabled_count()) {
            self.log
                .log(|| format!("start ({origin:?}) refused: no speaker is on"));
            self.post(NoticeSubject::Start, NoticeMessage::NoSpeaker);
            return false;
        }
        self.log.log(|| {
            format!(
                "start ({origin:?}): {}, {} speaker(s) on",
                self.chosen_name(),
                self.enabled_count()
            )
        });
        let started = self
            .chosen_source()
            .and_then(|source| self.audio.start(&source));
        match started {
            Ok(()) => {
                self.notices
                    .clear(&[NoticeSubject::Start, NoticeSubject::Source]);
                // Outputs that couldn't start are reported at the next poll;
                // take them now.
                self.take_events();
                true
            }
            Err(error) => {
                self.log
                    .log(|| format!("start ({origin:?}) failed: {error}"));
                self.gate.start_failed(origin);
                self.audio.stop();
                let message = self.start_problem(&error);
                self.post(NoticeSubject::Start, message);
                false
            }
        }
    }

    /// What a notice says about a start that failed with `error`.
    fn start_problem(&self, error: &BackendError) -> NoticeMessage {
        let app = matches!(self.chosen, Chosen::App { .. });
        match error {
            BackendError::SourceGone if app => NoticeMessage::SourceNotRunning {
                source: self.chosen_name(),
            },
            BackendError::OwnProcess | BackendError::ContainsAudioSync if app => {
                NoticeMessage::SourceRefused {
                    source: self.chosen_name(),
                }
            }
            _ => NoticeMessage::CaptureFailed,
        }
    }

    /// An output couldn't start, and the audio backend turned it off: tells
    /// the user, and stops streaming when nothing else is on (S69, S136).
    fn output_failed(&mut self, endpoint_id: &str, by_user: bool) {
        let reaction = reaction(&Incident::OutputStartFailed {
            uid: endpoint_id.to_owned(),
            by_user,
            enabled_left: self.enabled_count(),
        });
        if let Some(subject) = reaction.notice {
            let speaker = self.output_name(endpoint_id);
            self.post(subject, NoticeMessage::SpeakerFailed { speaker });
        }
        if reaction.stop_session {
            self.log.log(|| {
                format!(
                    "streaming stops: {} couldn't start and no other speaker is on",
                    self.output_name(endpoint_id)
                )
            });
            self.audio.stop();
        }
    }

    /// An AirPlay speaker can't play from AudioSync, so it is off: tells the
    /// user, and stops streaming when nothing else is on (S109, S136).
    fn airplay_cant_play(&mut self, id: &str, problem: AirPlayProblem) {
        let reason = match problem {
            AirPlayProblem::PasswordNotYet => DropReason::PasswordUnsupported,
            AirPlayProblem::PairingNotYet
            | AirPlayProblem::NeedsPin
            | AirPlayProblem::PairingFailed => DropReason::NeedsCode,
            _ => DropReason::Unreachable,
        };
        let reaction = reaction(&Incident::SpeakerDropped {
            id: id.to_owned(),
            reason,
            enabled_left: self.enabled_count(),
        });
        if let Some(subject) = reaction.notice {
            let speaker = self.airplay_name(id);
            self.post(
                subject,
                NoticeMessage::AirPlayCantPlay {
                    speaker,
                    problem: ui_problem(problem),
                },
            );
        }
        if reaction.stop_session && self.running() {
            self.log.log(|| {
                format!(
                    "streaming stops: {} can't play and no other speaker is on",
                    self.airplay_name(id)
                )
            });
            self.audio.stop();
        }
    }

    /// The user switched an AirPlay speaker on or off. One that can't play
    /// stays off, with a notice saying why.
    fn set_airplay_enabled(&mut self, id: &str, on: bool) {
        if on
            && !self
                .audio
                .airplay_speakers()
                .iter()
                .any(|speaker| speaker.id == id)
        {
            self.log
                .log(|| format!("AirPlay speaker ({id}) isn't listed; nothing to switch on"));
            return;
        }
        self.log.log(|| {
            let state = if on { "on" } else { "off" };
            format!("the user switched {} {state}", self.airplay_name(id))
        });
        if on {
            // A new attempt.
            self.notices.clear(&[airplay_subject(id)]);
        }
        if let Err(problem) = self.audio.set_airplay_enabled(id, on) {
            self.airplay_switch_failed(id, problem);
            return;
        }
        if on {
            self.start_if_switched_on();
        } else {
            self.stop_if_idle();
        }
        self.take_events();
    }

    /// An AirPlay speaker the user switched on didn't turn on: an Apple TV
    /// that asks for a PIN is paired (it turns on once paired); any other
    /// stays off with a notice.
    fn airplay_switch_failed(&mut self, id: &str, problem: AirPlayProblem) {
        if problem == AirPlayProblem::NeedsPin {
            self.log.log(|| {
                format!(
                    "{} needs pairing with a PIN: the pairing starts",
                    self.airplay_name(id)
                )
            });
            if let Err(problem) = self.audio.start_airplay_pairing(id) {
                self.airplay_cant_play(id, problem);
            }
            return;
        }
        self.airplay_cant_play(id, problem);
    }

    /// Stops streaming when a speaker was switched off and none is left on
    /// (S118, S162).
    fn stop_if_idle(&mut self) {
        let action = self.streaming_after(SpeakerEvent::SwitchedOff);
        if action == StreamingAction::Stop {
            self.log
                .log(|| "streaming stops: no speaker is on anymore".to_owned());
            self.audio.stop();
        }
    }

    /// Starts streaming when a speaker was switched on while nothing streams
    /// (S119, S162).
    fn start_if_switched_on(&mut self) {
        let action = self.streaming_after(SpeakerEvent::SwitchedOn);
        if action == StreamingAction::Start {
            self.start_session(StartOrigin::User);
        }
    }

    /// Takes in what the audio backend reports. Returns whether it reported
    /// anything.
    fn take_events(&mut self) -> bool {
        let events = self.audio.poll();
        let any = !events.is_empty();
        for event in events {
            self.handle(event);
        }
        any
    }

    fn handle(&mut self, event: BackendEvent) {
        match event {
            BackendEvent::OutputsChanged => {
                self.outputs_listed();
                self.migrate_delays_if_due();
            }
            // It stays on, and plays again once its device is back. A device
            // still listed only lost its stream (after a format change, say):
            // the next poll opens it again, so that needs no notice.
            BackendEvent::OutputLost { id } => {
                if !self.audio.outputs().iter().any(|output| output.id == id) {
                    let speaker = self.output_name(&id);
                    self.post(speaker_subject(&id), NoticeMessage::SpeakerLost { speaker });
                }
            }
            BackendEvent::OutputFailed { id, .. } => self.output_failed(&id, false),
            BackendEvent::Stopped { reason } => {
                let message = match reason {
                    StopReason::SourceQuit => {
                        // A dismissed double-audio notice was about this run
                        // of the app.
                        self.heard_twice_dismissed.remove(&self.chosen.key());
                        NoticeMessage::SourceQuit {
                            source: self.chosen_name(),
                        }
                    }
                    StopReason::CaptureFailed(_) => NoticeMessage::CaptureLost,
                };
                self.post(NoticeSubject::Source, message);
                self.refresh_sources();
            }
            BackendEvent::HeardTwice { paths } => self.heard_twice_changed(paths),
            BackendEvent::AirPlayChanged => self.airplay_listed(),
            BackendEvent::AirPlayPlaying { id } => {
                self.clear_if(airplay_subject(&id), |message| {
                    matches!(message, NoticeMessage::AirPlayRetrying { .. })
                });
            }
            BackendEvent::AirPlayTrouble { id, problem } => {
                let speaker = self.airplay_name(&id);
                self.post(
                    airplay_subject(&id),
                    NoticeMessage::AirPlayRetrying {
                        speaker,
                        problem: ui_problem(problem),
                    },
                );
            }
            BackendEvent::AirPlayFailed { id, problem } => self.airplay_cant_play(&id, problem),
            BackendEvent::AirPlayPaired { id } => {
                self.log
                    .log(|| format!("{} is paired and on", self.airplay_name(&id)));
                self.notices.clear(&[airplay_subject(&id)]);
                self.start_if_switched_on();
            }
        }
    }

    /// Takes in where the source is heard straight now: see the module
    /// documentation.
    fn heard_twice_changed(&mut self, paths: Vec<DirectPath>) {
        let subject = HeardTwiceSubject {
            source: self.chosen.key(),
            endpoints: paths
                .iter()
                .map(|path| (path.endpoint_id.clone(), path.endpoint_name.clone()))
                .collect(),
        };
        let dismissed = !paths.is_empty()
            && self
                .heard_twice_dismissed
                .get(&subject.source)
                .is_some_and(|dismissed| {
                    paths
                        .iter()
                        .all(|path| dismissed.contains(&path.endpoint_id))
                });
        if paths.is_empty() || dismissed {
            self.heard_twice = None;
            if self.notices.clear(&[NoticeSubject::DoubleAudio]) {
                self.log.log(|| {
                    "notice about DoubleAudio cleared: the source is heard only through \
                     AudioSync"
                        .to_owned()
                });
            }
            if dismissed {
                self.log.log(|| {
                    format!(
                        "no notice about DoubleAudio: the user dismissed it for {:?}",
                        subject.endpoints
                    )
                });
            }
            return;
        }
        let names = |plays_here: bool| -> Vec<String> {
            paths
                .iter()
                .filter(|path| path.audiosync_plays_here == plays_here)
                .map(|path| path.endpoint_name.clone())
                .collect()
        };
        let message = NoticeMessage::HeardTwice {
            source: match &self.chosen {
                Chosen::WholeSystem => None,
                Chosen::App { name, .. } => Some(name.clone()),
            },
            mute: names(false),
            shared: names(true),
            default_output: paths
                .iter()
                .find(|path| path.reason == DirectReason::DefaultEndpoint)
                .map(|path| path.endpoint_name.clone()),
        };
        let showing = self.heard_twice.as_ref() == Some(&subject)
            && self.notices.get(&NoticeSubject::DoubleAudio).is_some();
        if !showing {
            self.post(NoticeSubject::DoubleAudio, message);
        } else if let Some(posted) = self.notices.get_mut(&NoticeSubject::DoubleAudio)
            && posted.message != message
        {
            // The same outputs: only which of them AudioSync plays to
            // changed, so the notice changes where it is.
            self.log
                .log(|| format!("notice about DoubleAudio changed: {message:?}"));
            posted.message = message;
        }
        self.heard_twice = Some(subject);
    }

    /// Remembers that the user dismissed the double-audio notice showing.
    fn remember_heard_twice_dismissal(&mut self) {
        let Some(subject) = self.heard_twice.take() else {
            return;
        };
        self.log.log(|| {
            format!(
                "notice about DoubleAudio dismissed: it stays away for {} and {:?}",
                subject.source, subject.endpoints
            )
        });
        self.heard_twice_dismissed
            .entry(subject.source)
            .or_default()
            .extend(subject.endpoints.into_iter().map(|(id, _)| id));
    }

    /// The sources as the UI lists them: what is listed, and the chosen app
    /// after them while it isn't running, so the choice still shows.
    fn source_rows(&self) -> Vec<Source> {
        let mut rows: Vec<Source> = self
            .sources
            .iter()
            .map(|source| Source {
                id: source_key(source),
                name: source.name.clone(),
                kind: match source.id {
                    SourceId::WholeSystem => SourceKind::System,
                    SourceId::App { .. } => SourceKind::App,
                },
            })
            .collect();
        let chosen = self.chosen.key();
        if !rows.iter().any(|row| row.id == chosen) {
            rows.push(Source {
                id: chosen,
                name: self.chosen_name(),
                kind: SourceKind::App,
            });
        }
        rows
    }

    fn airplay_speaker(&self, speaker: AirPlayInfo) -> Speaker {
        let id = airplay_key(&speaker.id);
        Speaker {
            auto_connect: self.auto_connect.contains(&id),
            id,
            name: speaker.name,
            kind: SpeakerKind::AirPlay,
            enabled: speaker.enabled,
            volume: speaker.volume,
            delay_ms: None,
            availability: if speaker.pairing {
                Availability::Pairing
            } else {
                availability(speaker.availability)
            },
            paired: speaker.paired,
        }
    }

    /// A pairing's PIN prompt as the UI shows it.
    fn pin_prompt(&self, prompt: AirPlayPinPrompt) -> PinPrompt {
        PinPrompt {
            speaker_id: airplay_key(&prompt.id),
            name: prompt.name,
            phase: match prompt.phase {
                PairingPhase::Starting => PinPhase::Starting,
                PairingPhase::WaitingForPin => PinPhase::Ready,
                PairingPhase::Checking => PinPhase::Checking,
            },
            wrong_pin: prompt.wrong_pin,
        }
    }

    fn speaker(&self, output: OutputInfo) -> Speaker {
        let id = speaker_key(&output.id);
        Speaker {
            auto_connect: self.auto_connect.contains(&id),
            id,
            name: output.name,
            kind: speaker_kind(output.kind),
            enabled: output.enabled,
            volume: output.volume,
            // The reference has no delay setting (spec S181).
            delay_ms: (!output.is_reference).then_some(output.delay_ms),
            availability: Availability::Open,
            paired: false,
        }
    }

    /// The AirPlay delay field's value: L = L_s + A, while an AirPlay sender
    /// runs (spec S178, S181).
    fn airplay_delay_ms(&self) -> Option<f64> {
        self.audio
            .airplay_sender_latency_ms()
            .map(|sender| native_airplay_latency_ms(sender, self.audio.airplay_adjustment()))
    }

    /// The one-time migration of a format 1 settings file's delays (spec
    /// S180), once an output list is in: the saved trims become offsets
    /// relative to the reference, the reference's trim becomes the AirPlay
    /// delay adjustment, and the file is saved in the new format.
    fn migrate_delays_if_due(&mut self) {
        if !self.delays_to_migrate {
            return;
        }
        self.delays_to_migrate = false;
        let reference = self
            .audio
            .outputs()
            .into_iter()
            .find(|output| output.is_reference);
        let mut migrated = self.saved.clone();
        settings::migrate_delays(&mut migrated, reference.as_ref().map(|r| r.id.as_str()));
        for (id, speaker) in &migrated.outputs {
            self.audio.set_output_delay(id, speaker.delay_ms);
        }
        self.audio
            .set_airplay_adjustment(migrated.airplay_adjustment_ms);
        self.log.log(|| {
            let relative_to = reference.as_ref().map_or_else(
                || "no reference output".to_owned(),
                |r| format!("{} ({})", r.name, r.id),
            );
            format!(
                "settings: {} output delay(s) migrated to offsets relative to {relative_to}; \
                 AirPlay delay adjustment {} ms",
                migrated.outputs.len(),
                migrated.airplay_adjustment_ms
            )
        });
        self.saved = migrated;
        self.must_save = true;
        self.persist();
    }
}

impl<A: AudioBackend> Backend for AudioAdapter<A> {
    fn snapshot(&self) -> AppState {
        let reference_name = self
            .audio
            .outputs()
            .into_iter()
            .find(|output| output.is_reference)
            .map(|output| output.name);
        AppState {
            reference_name,
            airplay_delay_ms: self.airplay_delay_ms(),
            running: self.running(),
            sources: self.source_rows(),
            source_id: self.chosen.key(),
            master_volume: self.audio.master_volume(),
            speakers: self
                .audio
                .outputs()
                .into_iter()
                .map(|output| self.speaker(output))
                .chain(
                    self.audio
                        .airplay_speakers()
                        .into_iter()
                        .map(|speaker| self.airplay_speaker(speaker)),
                )
                .collect(),
            groups: self.groups.clone(),
            equalizer: self.equalizer,
            notices: self
                .notices
                .iter()
                .map(|(subject, posted)| Notice {
                    id: posted.id,
                    subject: format!("{subject:?}"),
                    message: posted.message.clone(),
                    time: posted.time.clone(),
                })
                .collect(),
            pin_prompt: self
                .audio
                .airplay_pin_prompt()
                .map(|prompt| self.pin_prompt(prompt)),
        }
    }

    fn set_running(&mut self, on: bool) {
        self.log.log(|| {
            let state = if on { "on" } else { "off" };
            format!("the user switched streaming {state}")
        });
        if !on {
            if self.running() {
                self.gate.user_switched_off();
                self.audio.stop();
            }
            return;
        }
        let action = self.streaming_after(SpeakerEvent::StartRequested);
        match action {
            // Without a speaker on, the start asks for one.
            StreamingAction::Start | StreamingAction::AskForSpeaker => {
                self.start_session(StartOrigin::User);
            }
            StreamingAction::Stop => self.audio.stop(),
            StreamingAction::Nothing => {}
        }
    }

    /// A new source clears the old one's notices; while streaming, capture
    /// starts again from it (S141, S163).
    fn select_source(&mut self, id: &str) {
        let Some(picked) = self
            .sources
            .iter()
            .find(|source| source_key(source) == id)
            .map(Chosen::from_source)
        else {
            return;
        };
        if picked.key() == self.chosen.key() {
            return;
        }
        self.log.log(|| {
            let name = match &picked {
                Chosen::WholeSystem => WHOLE_SYSTEM_NAME,
                Chosen::App { name, .. } => name,
            };
            format!("source chosen: {name} ({})", picked.key())
        });
        self.chosen = picked;
        self.notices
            .clear(&[NoticeSubject::Source, NoticeSubject::Capture]);
        // A start ends the running session by itself and keeps the AirPlay
        // speakers in the stream, restarting only their timeline (S141); a
        // stop would tear them down and connect them again.
        let switch = source_switch(self.running());
        let started = switch
            .start
            .is_some_and(|origin| self.start_session(origin));
        if switch.stop_keeping_airplay && !started && self.running() {
            self.audio.stop();
        }
    }

    fn set_master_volume(&mut self, position: f64) {
        self.audio.set_master_volume(position);
    }

    fn set_speaker_enabled(&mut self, id: &str, on: bool) {
        let endpoint_id = match SpeakerKey::parse(id) {
            Some(SpeakerKey::Local(endpoint_id)) => endpoint_id,
            Some(SpeakerKey::AirPlay(airplay_id)) => {
                self.set_airplay_enabled(&airplay_id, on);
                return;
            }
            None => return,
        };
        self.log.log(|| {
            let state = if on { "on" } else { "off" };
            format!(
                "the user switched {} {state}",
                self.output_name(&endpoint_id)
            )
        });
        if on {
            // A new attempt.
            self.notices.clear(&[speaker_subject(&endpoint_id)]);
        }
        let result = self.audio.set_output_enabled(&endpoint_id, on);
        if result.is_err() {
            self.output_failed(&endpoint_id, true);
        }
        if on {
            self.start_if_switched_on();
        } else {
            self.stop_if_idle();
        }
        self.take_events();
    }

    fn set_speaker_volume(&mut self, id: &str, position: f64) {
        match SpeakerKey::parse(id) {
            Some(SpeakerKey::Local(endpoint_id)) => {
                self.audio.set_output_volume(&endpoint_id, position);
            }
            Some(SpeakerKey::AirPlay(airplay_id)) => {
                self.audio.set_airplay_volume(&airplay_id, position);
            }
            None => {}
        }
    }

    /// A local output's delay, or an AirPlay speaker's own delay d_s.
    fn set_speaker_delay(&mut self, id: &str, ms: f64) {
        match SpeakerKey::parse(id) {
            Some(SpeakerKey::Local(endpoint_id)) => {
                self.audio
                    .set_output_delay(&endpoint_id, clamp_delay_ms(ms));
            }
            Some(SpeakerKey::AirPlay(airplay_id)) => {
                self.audio
                    .set_airplay_delay(&airplay_id, clamp_delay_ms(ms));
            }
            None => {}
        }
    }

    /// Stores the adjustment that makes L what the user entered, with the
    /// sender's latency of this moment (spec S178, S181).
    fn set_airplay_delay(&mut self, ms: f64) {
        let Some(sender) = self.audio.airplay_sender_latency_ms() else {
            return;
        };
        if let Some(adjustment) = native_airplay_adjustment_ms(ms, sender) {
            self.audio.set_airplay_adjustment(adjustment);
        }
    }

    fn set_auto_connect(&mut self, id: &str, on: bool) {
        self.log.log(|| {
            let state = if on { "on" } else { "off" };
            format!("auto-connect {state} for {id}")
        });
        if on {
            self.auto_connect.insert(id.to_owned());
        } else {
            self.auto_connect.remove(id);
        }
    }

    /// Present members only; absent ones are left alone.
    fn toggle_group(&mut self, id: &str) {
        let state = self.snapshot();
        let Some(group) = state.groups.iter().find(|group| group.id == id) else {
            return;
        };
        let members: Vec<SpeakerKey> = state
            .available_members(group)
            .filter_map(|speaker| SpeakerKey::parse(&speaker.id))
            .collect();
        if members.is_empty() {
            return;
        }
        // While streaming is off, a click starts it, even for a group that
        // is on already.
        let turn_on = state.group_state(group).click_turns_on();
        self.log.log(|| {
            let switched = if turn_on { "on" } else { "off" };
            format!(
                "the user switched group \"{}\" {switched}: {} speaker(s) present",
                group.name,
                members.len()
            )
        });
        for member in &members {
            match member {
                SpeakerKey::Local(endpoint_id) => {
                    if turn_on {
                        if self.running() && self.is_enabled(endpoint_id) {
                            continue;
                        }
                        // A new attempt.
                        self.notices.clear(&[speaker_subject(endpoint_id)]);
                    }
                    if self.audio.set_output_enabled(endpoint_id, turn_on).is_err() {
                        self.output_failed(endpoint_id, true);
                    }
                }
                SpeakerKey::AirPlay(id) => {
                    if turn_on {
                        if self.running() && self.is_airplay_enabled(id) {
                            continue;
                        }
                        self.notices.clear(&[airplay_subject(id)]);
                    }
                    if let Err(problem) = self.audio.set_airplay_enabled(id, turn_on) {
                        self.airplay_switch_failed(id, problem);
                    }
                }
            }
        }
        if turn_on {
            self.start_if_switched_on();
        } else {
            self.stop_if_idle();
        }
        self.take_events();
    }

    fn new_group(&mut self, name: &str) -> Result<(), GroupError> {
        let members: Vec<String> = self
            .audio
            .outputs()
            .into_iter()
            .filter(|output| output.enabled)
            .map(|output| speaker_key(&output.id))
            .chain(
                self.audio
                    .airplay_speakers()
                    .into_iter()
                    .filter(|speaker| speaker.enabled)
                    .map(|speaker| airplay_key(&speaker.id)),
            )
            .collect();
        if members.is_empty() {
            return Err(GroupError::NoActiveSpeakers);
        }
        self.log.log(|| {
            format!(
                "group \"{}\" made of {} speaker(s)",
                name.trim(),
                members.len()
            )
        });
        self.groups.push(Group {
            id: format!("group:{}", self.next_group),
            name: name.trim().to_owned(),
            members,
        });
        self.next_group += 1;
        Ok(())
    }

    fn rename_group(&mut self, id: &str, name: &str) {
        if let Some(group) = self.groups.iter_mut().find(|group| group.id == id) {
            group.name = renamed_group(&group.name, name);
        }
    }

    fn delete_group(&mut self, id: &str) {
        self.groups.retain(|group| group.id != id);
    }

    fn set_equalizer(&mut self, settings: EqSettings) {
        self.equalizer = settings.quantized();
        self.audio.set_equalizer(self.equalizer);
    }

    fn dismiss_notice(&mut self, id: u64) {
        let subject = self
            .notices
            .iter()
            .find(|(_, posted)| posted.id == id)
            .map(|(subject, _)| subject.clone());
        if let Some(subject) = subject {
            if subject == NoticeSubject::DoubleAudio {
                self.remember_heard_twice_dismissal();
            }
            self.notices.clear(&[subject]);
        }
    }

    fn dismiss_all_notices(&mut self) {
        if self.notices.get(&NoticeSubject::DoubleAudio).is_some() {
            self.remember_heard_twice_dismissal();
        }
        self.notices.clear_all();
    }

    fn refresh_devices(&mut self) {
        self.log
            .log(|| "the user asked to list the devices again".to_owned());
        self.list_devices();
    }

    /// The PIN itself is never logged.
    fn submit_pin(&mut self, id: &str, pin: &str) {
        let Some(SpeakerKey::AirPlay(airplay_id)) = SpeakerKey::parse(id) else {
            return;
        };
        let sent = self.audio.submit_airplay_pin(&airplay_id, pin);
        self.log.log(|| {
            let what = if sent { "sent" } else { "not taken" };
            format!(
                "the user typed the PIN for {}: {what}",
                self.airplay_name(&airplay_id)
            )
        });
    }

    fn cancel_pairing(&mut self, id: &str) {
        let Some(SpeakerKey::AirPlay(airplay_id)) = SpeakerKey::parse(id) else {
            return;
        };
        self.log.log(|| {
            format!(
                "the user cancelled the pairing with {}",
                self.airplay_name(&airplay_id)
            )
        });
        self.audio.cancel_airplay_pairing(&airplay_id);
        self.take_events();
    }

    fn forget_pairing(&mut self, id: &str) {
        let Some(SpeakerKey::AirPlay(airplay_id)) = SpeakerKey::parse(id) else {
            return;
        };
        self.log.log(|| {
            format!(
                "the user forgot the pairing with {}",
                self.airplay_name(&airplay_id)
            )
        });
        self.audio.forget_airplay_pairing(&airplay_id);
        self.notices.clear(&[airplay_subject(&airplay_id)]);
        self.stop_if_idle();
        self.take_events();
    }

    /// Also writes the settings file when anything changed since the last
    /// poll, so a slider drag is saved about twice a second at most.
    fn poll(&mut self) -> bool {
        let mut changed = self.take_events();
        self.polls += 1;
        if self.polls >= SOURCE_POLLS {
            self.polls = 0;
            changed |= self.refresh_sources();
        }
        self.persist();
        changed
    }

    fn view_settings(&self) -> ViewSettings {
        self.view
    }

    fn set_view_settings(&mut self, settings: ViewSettings) {
        self.view = settings;
    }

    fn save_settings(&mut self) {
        self.persist();
    }
}

/// The kind a local output shows as; "This PC" lists the built-in ones.
fn speaker_kind(kind: OutputKind) -> SpeakerKind {
    match kind {
        OutputKind::BuiltIn => SpeakerKind::BuiltIn,
        OutputKind::Headphones => SpeakerKind::Headphones,
        OutputKind::Bluetooth => SpeakerKind::Bluetooth,
        OutputKind::Usb => SpeakerKind::Usb,
        OutputKind::Hdmi => SpeakerKind::Hdmi,
        OutputKind::DisplayPort => SpeakerKind::DisplayPort,
        OutputKind::Virtual => SpeakerKind::Virtual,
        OutputKind::Other => SpeakerKind::Other,
    }
}

/// The UI's ID of a local output.
fn speaker_key(endpoint_id: &str) -> String {
    SpeakerKey::Local(endpoint_id.to_owned()).to_string()
}

/// The UI's ID of an AirPlay speaker.
fn airplay_key(id: &str) -> String {
    SpeakerKey::AirPlay(id.to_owned()).to_string()
}

/// What a notice about one local output is about.
fn speaker_subject(endpoint_id: &str) -> NoticeSubject {
    NoticeSubject::Speaker(SpeakerKey::Local(endpoint_id.to_owned()))
}

/// What a notice about one AirPlay speaker is about.
fn airplay_subject(id: &str) -> NoticeSubject {
    NoticeSubject::Speaker(SpeakerKey::AirPlay(id.to_owned()))
}

/// How an AirPlay speaker's row shows whether it can play.
fn availability(availability: AirPlayAvailability) -> Availability {
    match availability {
        AirPlayAvailability::Open => Availability::Open,
        AirPlayAvailability::PasswordNotYet => Availability::PasswordNotYet,
        AirPlayAvailability::NeedsPin => Availability::NeedsCode,
        AirPlayAvailability::PairingNotYet => Availability::PairingNotYet,
        AirPlayAvailability::AirPlay2NotYet => Availability::AirPlay2NotYet,
        AirPlayAvailability::Unsupported => Availability::Unsupported,
    }
}

/// An AirPlay problem as a notice words it.
fn ui_problem(problem: AirPlayProblem) -> ui_model::AirPlayProblem {
    use ui_model::AirPlayProblem as P;
    match problem {
        AirPlayProblem::NotResponding => P::NotResponding,
        AirPlayProblem::ConnectionLost => P::ConnectionLost,
        AirPlayProblem::Busy => P::Busy,
        AirPlayProblem::Refused => P::Refused,
        AirPlayProblem::PasswordNotYet => P::PasswordNotYet,
        AirPlayProblem::NeedsPin => P::NeedsPin,
        AirPlayProblem::PairingFailed => P::PairingFailed,
        AirPlayProblem::PairingNotYet => P::PairingNotYet,
        AirPlayProblem::AirPlay2NotYet => P::AirPlay2NotYet,
        AirPlayProblem::Unsupported => P::Unsupported,
    }
}

/// A listed source as the debug log names it.
fn describe_source(source: &SourceInfo) -> String {
    match source.id {
        SourceId::WholeSystem => source.name.clone(),
        SourceId::App { root_pid } => format!(
            "{} ({}, process {root_pid})",
            source.name, source.executable
        ),
    }
}

/// The UI's ID of a listed source.
fn source_key(source: &SourceInfo) -> String {
    match source.id {
        SourceId::WholeSystem => SYSTEM_KEY.to_owned(),
        SourceId::App { root_pid } => app_key(root_pid, &source.executable),
    }
}

/// The UI's ID of an app source. Executable names can't hold a colon.
fn app_key(root_pid: u32, executable: &str) -> String {
    format!("app:{root_pid}:{executable}")
}

#[cfg(test)]
mod tests {
    use audiosync_core::volume::{DEFAULT_LOCAL_POSITION, output_gain};

    use audiosync_airplay::discovery::Device;
    use audiosync_airplay::error::{ErrorKind, SessionError};
    use audiosync_airplay::pairing::PairingEvent;
    use audiosync_airplay::sender::SpeakerState as AirPlayState;

    use super::*;
    use crate::audio::airplay::tests::{APPLE_TV_TXT, Call, FakeAirPlay, OPEN_TXT, device, id_of};
    use crate::audio::capture::LoopbackMode;
    use crate::audio::mock::{MOCK_OWN_PID, MockBackend};
    use crate::audio::pairings::tests::credentials;
    use crate::audio::routing::HOLD_CHECKS;
    use crate::debug_log::{FILE_NAME, TEST_FLUSH, assert_in_order, read_messages, test_dir};
    use crate::ui_model::{MAX_DELAY_MS, MIN_DELAY_MS, SpeakerState};

    const SPEAKERS: &str = "speakers";
    const HEADPHONES: &str = "headphones";
    const PLAYER_PID: u32 = 2000;

    /// Two outputs, both off, and one app, as at startup.
    fn adapter() -> AudioAdapter<MockBackend> {
        let mut audio = MockBackend::empty();
        audio.connect_output(SPEAKERS, "Speakers");
        audio.connect_output(HEADPHONES, "Headphones");
        audio.add_app(PLAYER_PID, "player.exe");
        AudioAdapter::new(audio, || "09.41".to_owned(), DebugLog::default())
    }

    /// The UI's ID of output `endpoint_id`.
    fn key(endpoint_id: &str) -> String {
        format!("lc:{endpoint_id}")
    }

    fn player() -> String {
        format!("app:{PLAYER_PID}:player.exe")
    }

    fn turn_on(adapter: &mut AudioAdapter<MockBackend>, endpoint_id: &str) {
        adapter.set_speaker_enabled(&key(endpoint_id), true);
    }

    fn messages(adapter: &AudioAdapter<MockBackend>) -> Vec<NoticeMessage> {
        adapter
            .snapshot()
            .notices
            .into_iter()
            .map(|notice| notice.message)
            .collect()
    }

    fn enabled(adapter: &AudioAdapter<MockBackend>, endpoint_id: &str) -> bool {
        adapter
            .snapshot()
            .speaker(&key(endpoint_id))
            .is_some_and(|speaker| speaker.enabled)
    }

    fn playing(adapter: &AudioAdapter<MockBackend>) -> Vec<String> {
        adapter.audio.playing_ids()
    }

    fn captures(adapter: &AudioAdapter<MockBackend>) -> Vec<(LoopbackMode, u32)> {
        adapter
            .audio
            .driver()
            .captures_started()
            .iter()
            .map(|activation| (activation.mode, activation.target_pid))
            .collect()
    }

    #[test]
    fn the_snapshot_shows_the_sources_and_outputs_listed() {
        let state = adapter().snapshot();
        assert!(!state.running);
        let sources: Vec<(&str, &str, SourceKind)> = state
            .sources
            .iter()
            .map(|s| (s.id.as_str(), s.name.as_str(), s.kind))
            .collect();
        assert_eq!(
            sources,
            [
                ("system", "All System Audio", SourceKind::System),
                ("app:2000:player.exe", "Player", SourceKind::App),
            ]
        );
        assert_eq!(state.source_id, "system");
        let ids: Vec<&str> = state.speakers.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["lc:headphones", "lc:speakers"]);
        let speakers = state.speaker("lc:speakers").unwrap();
        assert_eq!(speakers.name, "Speakers");
        assert!(!speakers.enabled && !speakers.auto_connect);
        assert_eq!(speakers.delay_ms, Some(0.0), "local outputs have a delay");
        assert_eq!(speakers.volume, DEFAULT_LOCAL_POSITION);
        assert_eq!(speakers.availability, Availability::Open);
        assert!(state.notices.is_empty() && state.groups.is_empty());
    }

    #[test]
    fn built_in_outputs_are_listed_under_this_pc() {
        let mut audio = MockBackend::empty();
        audio.connect_output_as("tv", "TV", OutputKind::Hdmi);
        audio.connect_output_as("jack", "Headphones", OutputKind::Headphones);
        audio.connect_output_as("internal", "Speakers", OutputKind::BuiltIn);
        audio.connect_output_as("bt", "Buds", OutputKind::Bluetooth);
        let state = AudioAdapter::new(audio, String::new, DebugLog::default()).snapshot();
        let (this_pc, connected) = state.layout();
        let rows = |list: &[&Speaker]| -> Vec<(String, SpeakerKind)> {
            list.iter().map(|s| (s.name.clone(), s.kind)).collect()
        };
        assert_eq!(
            rows(&this_pc),
            [
                ("Headphones".to_owned(), SpeakerKind::Headphones),
                ("Speakers".to_owned(), SpeakerKind::BuiltIn)
            ]
        );
        assert_eq!(
            rows(&connected),
            [
                ("Buds".to_owned(), SpeakerKind::Bluetooth),
                ("TV".to_owned(), SpeakerKind::Hdmi)
            ]
        );
    }

    #[test]
    fn turning_a_speaker_on_starts_streaming_to_it() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        let state = adapter.snapshot();
        assert!(state.running);
        let speaker = state.speaker(&key(SPEAKERS)).unwrap();
        assert_eq!(state.speaker_state(speaker), SpeakerState::Playing);
        assert_eq!(playing(&adapter), [SPEAKERS]);
        assert_eq!(
            captures(&adapter),
            [(LoopbackMode::ExcludeTree, MOCK_OWN_PID)],
            "the whole system, without AudioSync"
        );
        turn_on(&mut adapter, HEADPHONES);
        assert_eq!(playing(&adapter), [HEADPHONES, SPEAKERS]);
        assert_eq!(captures(&adapter).len(), 1, "one session");
    }

    #[test]
    fn turning_the_last_speaker_off_stops_streaming() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        turn_on(&mut adapter, HEADPHONES);
        adapter.set_speaker_enabled(&key(SPEAKERS), false);
        assert!(adapter.snapshot().running);
        assert_eq!(playing(&adapter), [HEADPHONES]);
        adapter.set_speaker_enabled(&key(HEADPHONES), false);
        assert!(!adapter.snapshot().running);
        assert!(playing(&adapter).is_empty());
    }

    #[test]
    fn streaming_needs_a_speaker_that_is_on() {
        let mut adapter = adapter();
        adapter.set_running(true);
        assert!(!adapter.snapshot().running);
        assert_eq!(messages(&adapter), [NoticeMessage::NoSpeaker]);
        assert_eq!(adapter.snapshot().notices[0].time, "09.41");
        turn_on(&mut adapter, SPEAKERS);
        assert!(adapter.snapshot().running);
        assert!(messages(&adapter).is_empty(), "the start clears it");
    }

    #[test]
    fn switching_streaming_off_keeps_the_speakers_on() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        adapter.set_running(false);
        let state = adapter.snapshot();
        assert!(!state.running);
        let speaker = state.speaker(&key(SPEAKERS)).unwrap();
        assert_eq!(state.speaker_state(speaker), SpeakerState::WillPlay);
        adapter.set_running(true);
        assert_eq!(playing(&adapter), [SPEAKERS]);
    }

    #[test]
    fn volumes_delays_and_the_master_reach_the_audio_backend() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        turn_on(&mut adapter, HEADPHONES);
        adapter.set_speaker_volume(&key(SPEAKERS), 30.0);
        adapter.set_speaker_delay(&key(SPEAKERS), 120.4);
        adapter.set_master_volume(50.0);
        let state = adapter.snapshot();
        let speaker = state.speaker(&key(SPEAKERS)).unwrap();
        assert_eq!(speaker.volume, 30.0);
        assert_eq!(speaker.delay_ms, Some(120.0), "whole milliseconds");
        assert_eq!(state.master_volume, 50.0);
        let params = adapter.audio.playing_params(SPEAKERS).unwrap();
        assert_eq!(
            params.delay_frames, 5_760,
            "120 ms after the headphones at 0, at 48 kHz"
        );
        assert_eq!(params.gain, output_gain(30.0, 50.0) as f32);

        adapter.set_speaker_enabled(&key(HEADPHONES), false);
        adapter.set_speaker_delay(&key(HEADPHONES), 99_999.0);
        let state = adapter.snapshot();
        assert_eq!(
            state.speaker(&key(HEADPHONES)).unwrap().delay_ms,
            Some(MAX_DELAY_MS),
            "a delay set while off is kept, within the limit"
        );
        adapter.set_speaker_delay(&key(HEADPHONES), -99_999.0);
        let state = adapter.snapshot();
        assert_eq!(
            state.speaker(&key(HEADPHONES)).unwrap().delay_ms,
            Some(MIN_DELAY_MS),
            "and down to −2000 ms for a speaker that sounds late"
        );
    }

    #[test]
    fn a_group_switches_its_present_members_together() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        turn_on(&mut adapter, HEADPHONES);
        adapter.new_group(" Desk ").unwrap();
        let group = adapter.snapshot().groups[0].clone();
        assert_eq!(group.name, "Desk");
        assert_eq!(group.members, [key(HEADPHONES), key(SPEAKERS)]);

        adapter.toggle_group(&group.id);
        let state = adapter.snapshot();
        assert!(!state.running, "its speakers were the last ones on");
        assert_eq!(state.group_state(&group), SpeakerState::Off);

        adapter.toggle_group(&group.id);
        let state = adapter.snapshot();
        assert!(state.running);
        assert_eq!(state.group_state(&group), SpeakerState::Playing);
        assert_eq!(playing(&adapter), [HEADPHONES, SPEAKERS]);

        // An absent member is left alone.
        adapter.audio_mut().disconnect_output(HEADPHONES);
        adapter.poll();
        adapter.toggle_group(&group.id);
        assert!(!enabled(&adapter, SPEAKERS));
        assert_eq!(adapter.audio.enabled_outputs(), [HEADPHONES]);
        assert!(adapter.snapshot().running, "the absent member is still on");
    }

    #[test]
    fn a_new_group_needs_a_speaker_that_is_on_and_names_can_change() {
        let mut adapter = adapter();
        assert_eq!(
            adapter.new_group("Office"),
            Err(GroupError::NoActiveSpeakers)
        );
        turn_on(&mut adapter, HEADPHONES);
        adapter.new_group("Office").unwrap();
        adapter.new_group("Den").unwrap();
        let groups = || adapter.snapshot().groups;
        let ids: Vec<String> = groups().into_iter().map(|g| g.id).collect();
        assert_eq!(ids, ["group:1", "group:2"]);
        adapter.rename_group("group:1", "  ");
        adapter.rename_group("group:2", " Study ");
        let names: Vec<String> = adapter
            .snapshot()
            .groups
            .into_iter()
            .map(|g| g.name)
            .collect();
        assert_eq!(names, ["Office", "Study"], "a blank name keeps the old one");
        adapter.delete_group("group:1");
        assert_eq!(adapter.snapshot().groups.len(), 1);
    }

    #[test]
    fn an_app_that_quits_stops_streaming_and_stays_chosen() {
        let mut adapter = adapter();
        adapter.select_source(&player());
        turn_on(&mut adapter, SPEAKERS);
        assert_eq!(
            captures(&adapter),
            [(LoopbackMode::IncludeTree, PLAYER_PID)]
        );

        adapter.audio_mut().quit_app(PLAYER_PID);
        assert!(adapter.poll());
        let state = adapter.snapshot();
        assert!(!state.running);
        let quit = NoticeMessage::SourceQuit {
            source: "Player".into(),
        };
        assert_eq!(messages(&adapter), [quit]);
        assert_eq!(state.source_id, player(), "the choice stays");
        assert!(
            state
                .sources
                .iter()
                .any(|s| s.id == player() && s.name == "Player"),
            "and shows: {:?}",
            state.sources
        );
        assert!(enabled(&adapter, SPEAKERS), "the speakers stay on");

        adapter.set_running(true);
        assert!(!adapter.snapshot().running);
        let not_running = NoticeMessage::SourceNotRunning {
            source: "Player".into(),
        };
        assert_eq!(messages(&adapter), [not_running]);
        assert_eq!(captures(&adapter).len(), 1, "nothing started");
    }

    #[test]
    fn an_app_that_started_again_is_found_by_its_executable() {
        let mut adapter = adapter();
        adapter.select_source(&player());
        adapter.audio_mut().quit_app(PLAYER_PID);
        adapter.audio_mut().add_app(2100, "player.exe");
        turn_on(&mut adapter, SPEAKERS);
        assert!(adapter.snapshot().running);
        assert_eq!(captures(&adapter), [(LoopbackMode::IncludeTree, 2100)]);
        assert_eq!(adapter.snapshot().source_id, "app:2100:player.exe");
    }

    #[test]
    fn another_app_with_the_process_id_of_one_that_quit_isnt_captured() {
        let mut adapter = adapter();
        adapter.select_source(&player());
        adapter.audio_mut().quit_app(PLAYER_PID);
        adapter.audio_mut().add_app(PLAYER_PID, "other.exe");
        turn_on(&mut adapter, SPEAKERS);
        assert!(!adapter.snapshot().running);
        assert!(captures(&adapter).is_empty());
        let not_running = NoticeMessage::SourceNotRunning {
            source: "Player".into(),
        };
        assert_eq!(messages(&adapter), [not_running]);
        let state = adapter.snapshot();
        let ids: Vec<&str> = state.sources.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["system", "app:2000:other.exe", "app:2000:player.exe"]);
        assert_eq!(state.source_id, player());
    }

    #[test]
    fn an_app_without_an_audio_session_is_still_captured() {
        let mut adapter = adapter();
        adapter.select_source(&player());
        // Paused, it closed its stream, so the source list leaves it out.
        adapter.audio_mut().close_audio_session(PLAYER_PID);
        adapter.refresh_devices();
        assert_eq!(adapter.snapshot().source_id, player(), "the choice stays");

        turn_on(&mut adapter, SPEAKERS);
        assert!(adapter.snapshot().running);
        assert_eq!(
            captures(&adapter),
            [(LoopbackMode::IncludeTree, PLAYER_PID)]
        );
        assert!(messages(&adapter).is_empty(), "{:?}", messages(&adapter));
    }

    #[test]
    fn an_app_opened_again_is_captured_before_it_plays() {
        let mut adapter = adapter();
        adapter.select_source(&player());
        adapter.audio_mut().quit_app(PLAYER_PID);
        adapter.audio_mut().add_silent_app(2100, "player.exe");
        turn_on(&mut adapter, SPEAKERS);
        let state = adapter.snapshot();
        assert!(state.running);
        assert_eq!(captures(&adapter), [(LoopbackMode::IncludeTree, 2100)]);
        assert_eq!(state.source_id, "app:2100:player.exe");
        assert!(
            state
                .sources
                .iter()
                .any(|s| s.id == state.source_id && s.name == "Player"),
            "it shows: {:?}",
            state.sources
        );
        assert!(messages(&adapter).is_empty(), "{:?}", messages(&adapter));
    }

    #[test]
    fn a_new_source_while_streaming_starts_capture_again() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        adapter.select_source(&player());
        assert!(adapter.snapshot().running);
        assert_eq!(
            captures(&adapter),
            [
                (LoopbackMode::ExcludeTree, MOCK_OWN_PID),
                (LoopbackMode::IncludeTree, PLAYER_PID)
            ]
        );
        adapter.select_source(&player());
        assert_eq!(
            captures(&adapter).len(),
            2,
            "the same source changes nothing"
        );
    }

    #[test]
    fn capture_that_fails_while_streaming_stops_it_with_a_notice() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        adapter.audio_mut().fail_capture(BackendError::System {
            call: "IAudioCaptureClient::GetBuffer",
            code: 1,
        });
        assert!(adapter.poll());
        assert!(!adapter.snapshot().running);
        assert_eq!(messages(&adapter), [NoticeMessage::CaptureLost]);
    }

    #[test]
    fn capture_that_cant_start_leaves_streaming_off_with_a_notice() {
        let mut adapter = adapter();
        adapter
            .audio_mut()
            .refuse_captures(Some(BackendError::DeviceGone));
        turn_on(&mut adapter, SPEAKERS);
        assert!(!adapter.snapshot().running);
        assert!(enabled(&adapter, SPEAKERS), "the speaker stays on");
        assert_eq!(messages(&adapter), [NoticeMessage::CaptureFailed]);
    }

    #[test]
    fn start_problems_name_the_chosen_app() {
        let mut adapter = adapter();
        adapter.select_source(&player());
        let player = || "Player".to_owned();
        assert_eq!(
            adapter.start_problem(&BackendError::ContainsAudioSync),
            NoticeMessage::SourceRefused { source: player() }
        );
        assert_eq!(
            adapter.start_problem(&BackendError::OwnProcess),
            NoticeMessage::SourceRefused { source: player() }
        );
        assert_eq!(
            adapter.start_problem(&BackendError::SourceGone),
            NoticeMessage::SourceNotRunning { source: player() }
        );
        assert_eq!(
            adapter.start_problem(&BackendError::UnsupportedFormat),
            NoticeMessage::CaptureFailed
        );
    }

    #[test]
    fn an_output_that_cant_start_is_turned_off_with_a_notice() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        adapter
            .audio_mut()
            .fail_output(HEADPHONES, Some(BackendError::UnsupportedFormat));
        turn_on(&mut adapter, HEADPHONES);
        assert!(!enabled(&adapter, HEADPHONES));
        assert!(adapter.snapshot().running, "the speakers play on");
        let failed = NoticeMessage::SpeakerFailed {
            speaker: "Headphones".into(),
        };
        assert_eq!(messages(&adapter), [failed]);

        // Turning it on again is a new attempt.
        adapter.audio_mut().fail_output(HEADPHONES, None);
        turn_on(&mut adapter, HEADPHONES);
        assert!(messages(&adapter).is_empty());
        assert_eq!(playing(&adapter), [HEADPHONES, SPEAKERS]);
    }

    #[test]
    fn streaming_stops_when_the_only_output_cant_start() {
        let mut adapter = adapter();
        adapter
            .audio_mut()
            .fail_output(SPEAKERS, Some(BackendError::UnsupportedFormat));
        turn_on(&mut adapter, SPEAKERS);
        assert!(!adapter.snapshot().running);
        assert!(!enabled(&adapter, SPEAKERS));
        let failed = NoticeMessage::SpeakerFailed {
            speaker: "Speakers".into(),
        };
        assert_eq!(messages(&adapter), [failed]);
    }

    #[test]
    fn an_unplugged_output_is_reported_until_it_is_back() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        turn_on(&mut adapter, HEADPHONES);
        adapter.audio_mut().disconnect_output(HEADPHONES);
        assert!(adapter.poll());
        let state = adapter.snapshot();
        assert!(state.running, "the speakers play on");
        assert!(state.speaker(&key(HEADPHONES)).is_none(), "not listed");
        let lost = NoticeMessage::SpeakerLost {
            speaker: "Headphones".into(),
        };
        assert_eq!(messages(&adapter), [lost]);

        adapter.audio_mut().connect_output(HEADPHONES, "Headphones");
        assert!(adapter.poll());
        assert!(messages(&adapter).is_empty(), "its cause is gone");
        assert_eq!(playing(&adapter), [HEADPHONES, SPEAKERS]);
    }

    #[test]
    fn an_output_a_refresh_no_longer_lists_is_reported() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        turn_on(&mut adapter, HEADPHONES);
        // Its stream doesn't notice; the listing that the refresh asks for
        // does.
        adapter.audio_mut().disconnect_output_unnoticed(HEADPHONES);
        adapter.refresh_devices();
        let lost = NoticeMessage::SpeakerLost {
            speaker: "Headphones".into(),
        };
        assert_eq!(messages(&adapter), [lost]);
        assert_eq!(playing(&adapter), [SPEAKERS]);
    }

    #[test]
    fn a_stream_that_resets_while_its_device_stays_gets_no_notice() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        // A format change: the stream ends, and opening it again fails for a
        // moment as if the device were gone.
        adapter.audio_mut().end_renderer(SPEAKERS);
        adapter
            .audio_mut()
            .fail_output(SPEAKERS, Some(BackendError::DeviceGone));
        adapter.poll();
        assert!(playing(&adapter).is_empty());
        assert!(enabled(&adapter, SPEAKERS), "it stays on");
        assert!(messages(&adapter).is_empty(), "{:?}", messages(&adapter));

        adapter.audio_mut().fail_output(SPEAKERS, None);
        adapter.poll();
        adapter.poll();
        assert_eq!(playing(&adapter), [SPEAKERS]);
        assert!(messages(&adapter).is_empty(), "{:?}", messages(&adapter));
    }

    #[test]
    fn an_auto_connect_speaker_that_appears_is_turned_on_and_plays() {
        let mut adapter = adapter();
        adapter.set_auto_connect(&key(HEADPHONES), true);
        let state = adapter.snapshot();
        assert!(state.speaker(&key(HEADPHONES)).unwrap().auto_connect);
        assert!(
            !enabled(&adapter, HEADPHONES),
            "marking it turns nothing on"
        );

        adapter.audio_mut().disconnect_output(HEADPHONES);
        adapter.poll();
        adapter.audio_mut().connect_output(HEADPHONES, "Headphones");
        assert!(adapter.poll());
        assert!(enabled(&adapter, HEADPHONES));
        assert!(adapter.snapshot().running);
        assert_eq!(playing(&adapter), [HEADPHONES]);
    }

    #[test]
    fn auto_connect_doesnt_restart_streaming_the_user_switched_off() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        adapter.set_running(false);
        adapter.set_auto_connect(&key(HEADPHONES), true);
        adapter.audio_mut().disconnect_output(HEADPHONES);
        adapter.poll();
        adapter.audio_mut().connect_output(HEADPHONES, "Headphones");
        adapter.poll();
        assert!(
            enabled(&adapter, HEADPHONES),
            "on, to play at the next start"
        );
        assert!(!adapter.snapshot().running);
    }

    #[test]
    fn notices_can_be_dismissed_one_by_one_or_all_at_once() {
        let mut adapter = adapter();
        turn_on(&mut adapter, HEADPHONES);
        adapter
            .audio_mut()
            .fail_output(SPEAKERS, Some(BackendError::UnsupportedFormat));
        turn_on(&mut adapter, SPEAKERS);
        adapter.audio_mut().disconnect_output(HEADPHONES);
        adapter.poll();
        let notices = adapter.snapshot().notices;
        assert_eq!(notices.len(), 2, "{notices:?}");
        assert!(notices[0].id > notices[1].id, "newest first");
        adapter.dismiss_notice(notices[1].id);
        let left: Vec<u64> = adapter.snapshot().notices.iter().map(|n| n.id).collect();
        assert_eq!(left, [notices[0].id]);
        adapter.dismiss_all_notices();
        assert!(adapter.snapshot().notices.is_empty());
    }

    #[test]
    fn the_equalizer_is_quantized_and_reaches_the_capture() {
        let mut adapter = adapter();
        adapter.set_equalizer(EqSettings {
            enabled: true,
            preamp_db: 6.2,
            ..EqSettings::default()
        });
        assert_eq!(adapter.snapshot().equalizer.preamp_db, 6.0);
        turn_on(&mut adapter, SPEAKERS);
        let raised = 0.25 * 10f32.powf(6.0 / 20.0);
        let fed = adapter.audio_mut().feed_capture_level(4, 0.25);
        assert!(fed.iter().all(|s| (s - raised).abs() < 1e-4), "{fed:?}");
    }

    #[test]
    fn a_poll_reports_a_change_only_when_something_happened() {
        let mut adapter = adapter();
        assert!(!adapter.poll());
        adapter.audio_mut().connect_output("tv", "TV");
        assert!(adapter.poll());
        assert!(adapter.snapshot().speaker("lc:tv").is_some());
        assert!(!adapter.poll());
    }

    #[test]
    fn sources_are_listed_again_every_few_polls_and_on_refresh() {
        let mut adapter = adapter();
        adapter.audio_mut().add_app(3000, "music.exe");
        let names = |adapter: &AudioAdapter<MockBackend>| -> Vec<String> {
            let sources = adapter.snapshot().sources;
            sources.into_iter().map(|s| s.name).collect()
        };
        let changed: Vec<bool> = (0..SOURCE_POLLS).map(|_| adapter.poll()).collect();
        assert_eq!(changed.iter().filter(|&&c| c).count(), 1, "{changed:?}");
        assert_eq!(names(&adapter), ["All System Audio", "Music", "Player"]);

        adapter.audio_mut().add_app(4000, "video.exe");
        adapter.refresh_devices();
        assert_eq!(
            names(&adapter),
            ["All System Audio", "Music", "Player", "Video"]
        );
    }

    #[test]
    fn unknown_ids_change_nothing() {
        let mut adapter = adapter();
        let before = adapter.snapshot();
        adapter.set_speaker_enabled("lc:nothing-here", false);
        adapter.set_speaker_enabled("ap:kitchen", true);
        adapter.set_speaker_volume("garbage", 10.0);
        adapter.select_source("app:999999");
        adapter.toggle_group("group:9");
        adapter.dismiss_notice(42);
        assert_eq!(adapter.snapshot(), before);
    }

    #[test]
    fn user_actions_decisions_and_notices_are_logged() {
        let dir = test_dir("adapter-actions");
        let log = DebugLog::new(Some(dir.join(FILE_NAME)));
        let mut audio = MockBackend::empty();
        audio.connect_output(SPEAKERS, "Speakers");
        audio.connect_output(HEADPHONES, "Headphones");
        audio.add_app(PLAYER_PID, "player.exe");
        audio.set_debug_log(log.clone());
        let mut adapter = AudioAdapter::new(audio, || "09.41".to_owned(), log.clone());
        adapter.set_running(true);
        adapter.select_source(&player());
        adapter.set_speaker_enabled(&key(SPEAKERS), true);
        adapter.set_speaker_enabled(&key(SPEAKERS), false);
        assert!(log.flush(TEST_FLUSH));
        assert_in_order(
            &read_messages(&dir.join(FILE_NAME)),
            &[
                "outputs listed: 2",
                "sources listed: All System Audio; Player (player.exe, process 2000)",
                "source chosen: All System Audio (the default)",
                "the user switched streaming on",
                "start (User) refused: no speaker is on",
                "notice about Start: NoSpeaker",
                "source chosen: Player (app:2000:player.exe)",
                "the user switched Speakers on",
                "output \"Speakers\" (speakers) switched on",
                "start (User): Player, 1 speaker(s) on",
                "starting: capturing the app of process 2000",
                "capture started: process 2000 and its children",
                "output \"Speakers\" (speakers) started:",
                "the user switched Speakers off",
                "output \"Speakers\" (speakers) switched off",
                "output \"Speakers\" (speakers) stopped: switched off",
                "streaming stops: no speaker is on anymore",
                "stopping",
                "session ended:",
            ],
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    const TV: &str = "tv";
    const CABLE: &str = "cable";

    /// The double-audio notices showing.
    fn heard_twice(adapter: &AudioAdapter<MockBackend>) -> Vec<NoticeMessage> {
        adapter
            .snapshot()
            .notices
            .into_iter()
            .filter(|notice| matches!(notice.message, NoticeMessage::HeardTwice { .. }))
            .map(|notice| notice.message)
            .collect()
    }

    /// The double-audio notice for `source` (`None` for the whole system),
    /// naming the outputs to mute and the ones AudioSync plays to as well.
    fn twice(source: Option<&str>, mute: &[&str], shared: &[&str]) -> NoticeMessage {
        let names = |list: &[&str]| list.iter().map(|name| (*name).to_owned()).collect();
        NoticeMessage::HeardTwice {
            source: source.map(str::to_owned),
            mute: names(mute),
            shared: names(shared),
            default_output: None,
        }
    }

    /// The ID of the double-audio notice showing.
    fn heard_twice_id(adapter: &AudioAdapter<MockBackend>) -> Option<u64> {
        adapter
            .snapshot()
            .notices
            .into_iter()
            .find(|notice| matches!(notice.message, NoticeMessage::HeardTwice { .. }))
            .map(|notice| notice.id)
    }

    /// Runs the double-audio check now and takes in what it reports.
    fn check_routing(adapter: &mut AudioAdapter<MockBackend>) {
        adapter.audio_mut().check_routing_now();
        adapter.poll();
    }

    /// The adapter of [`adapter`] with a TV and a virtual cable connected,
    /// streaming the player to the speakers while it plays on the TV.
    fn player_on_the_tv() -> AudioAdapter<MockBackend> {
        let mut adapter = adapter();
        let audio = adapter.audio_mut();
        audio.connect_output_as(TV, "TV", OutputKind::Hdmi);
        audio.connect_output_as(CABLE, "Cable Input", OutputKind::Virtual);
        audio.play_on(PLAYER_PID, TV);
        adapter.refresh_devices();
        adapter.select_source(&player());
        turn_on(&mut adapter, SPEAKERS);
        assert!(adapter.snapshot().running);
        adapter
    }

    #[test]
    fn hearing_the_app_twice_raises_one_notice_that_follows_the_routing() {
        let mut adapter = player_on_the_tv();
        assert_eq!(heard_twice(&adapter), [twice(Some("Player"), &["TV"], &[])]);
        assert_eq!(
            adapter.snapshot().notices[0].subject,
            "DoubleAudio",
            "one subject"
        );
        // It plays on the speakers as well, which AudioSync plays to: the
        // TV can be muted, the speakers can't.
        adapter.audio_mut().play_on(PLAYER_PID, SPEAKERS);
        check_routing(&mut adapter);
        assert_eq!(
            heard_twice(&adapter),
            [twice(Some("Player"), &["TV"], &["Speakers"])]
        );
        // Moved in the volume mixer to a virtual cable nobody listens to, it
        // is heard once again after a few checks.
        adapter.audio_mut().pause_on(PLAYER_PID, SPEAKERS);
        adapter.audio_mut().move_session(PLAYER_PID, TV, CABLE);
        check_routing(&mut adapter);
        assert_eq!(heard_twice(&adapter).len(), 1, "held a few checks");
        for _ in 0..HOLD_CHECKS {
            check_routing(&mut adapter);
        }
        assert!(heard_twice(&adapter).is_empty());
    }

    #[test]
    fn the_double_audio_notice_clears_when_the_output_is_muted() {
        let mut adapter = player_on_the_tv();
        adapter.audio_mut().mute_output(TV, true);
        check_routing(&mut adapter);
        assert!(heard_twice(&adapter).is_empty());
        adapter.audio_mut().mute_output(TV, false);
        check_routing(&mut adapter);
        assert_eq!(heard_twice(&adapter), [twice(Some("Player"), &["TV"], &[])]);
    }

    #[test]
    fn the_double_audio_notice_clears_when_streaming_stops() {
        let mut adapter = player_on_the_tv();
        adapter.set_running(false);
        adapter.poll();
        assert!(!adapter.snapshot().running);
        assert!(heard_twice(&adapter).is_empty());
    }

    #[test]
    fn the_double_audio_notice_follows_a_new_source() {
        let mut adapter = player_on_the_tv();
        adapter.audio_mut().add_app(3000, "video.exe");
        adapter.refresh_devices();
        adapter.select_source("app:3000:video.exe");
        assert!(adapter.snapshot().running);
        assert!(
            heard_twice(&adapter).is_empty(),
            "the new app plays nowhere yet"
        );
        // The whole system takes in the player on the TV too.
        adapter.select_source(SYSTEM_KEY);
        assert_eq!(heard_twice(&adapter), [twice(None, &["TV"], &[])]);
    }

    #[test]
    fn the_whole_system_notice_names_the_default_output() {
        let mut adapter = player_on_the_tv();
        adapter.audio_mut().set_default_output(SPEAKERS);
        adapter.select_source(SYSTEM_KEY);
        // The speakers count as the default output, the TV because the
        // player plays there: the guide words each its own way.
        let expected = NoticeMessage::HeardTwice {
            source: None,
            mute: vec!["TV".to_owned()],
            shared: vec!["Speakers".to_owned()],
            default_output: Some("Speakers".to_owned()),
        };
        assert_eq!(heard_twice(&adapter), [expected]);
    }

    #[test]
    fn a_dismissed_double_audio_notice_comes_back_only_when_the_routing_changes() {
        let mut adapter = player_on_the_tv();
        let id = adapter.snapshot().notices[0].id;
        adapter.dismiss_notice(id);
        check_routing(&mut adapter);
        assert!(heard_twice(&adapter).is_empty(), "nothing changed");
        adapter.audio_mut().play_on(PLAYER_PID, SPEAKERS);
        check_routing(&mut adapter);
        assert_eq!(
            heard_twice(&adapter),
            [twice(Some("Player"), &["TV"], &["Speakers"])]
        );
    }

    #[test]
    fn turning_on_an_output_the_notice_names_changes_it_in_place() {
        let mut adapter = player_on_the_tv();
        let id = heard_twice_id(&adapter);
        turn_on(&mut adapter, TV);
        check_routing(&mut adapter);
        assert_eq!(heard_twice_id(&adapter), id, "not posted again");
        assert_eq!(heard_twice(&adapter), [twice(Some("Player"), &[], &["TV"])]);
        // Dismissed, it stays away while the output goes off and on again.
        adapter.dismiss_notice(id.unwrap());
        adapter.set_speaker_enabled(&key(TV), false);
        check_routing(&mut adapter);
        turn_on(&mut adapter, TV);
        check_routing(&mut adapter);
        assert!(heard_twice(&adapter).is_empty());
    }

    #[test]
    fn a_dismissed_double_audio_notice_stays_away_until_the_app_quits() {
        const BUDS: &str = "buds";
        let mut adapter = player_on_the_tv();
        adapter.dismiss_notice(heard_twice_id(&adapter).unwrap());
        // Streaming off and on again.
        adapter.set_running(false);
        adapter.poll();
        adapter.set_running(true);
        assert!(adapter.snapshot().running);
        assert!(heard_twice(&adapter).is_empty(), "after a restart");
        // Every speaker off, then on again.
        adapter.set_speaker_enabled(&key(SPEAKERS), false);
        adapter.poll();
        assert!(!adapter.snapshot().running);
        turn_on(&mut adapter, SPEAKERS);
        assert!(heard_twice(&adapter).is_empty(), "after the speakers");
        // Another output: a notice that names it.
        adapter
            .audio_mut()
            .connect_output_as(BUDS, "Buds", OutputKind::Bluetooth);
        adapter.audio_mut().play_on(PLAYER_PID, BUDS);
        check_routing(&mut adapter);
        assert_eq!(
            heard_twice(&adapter),
            [twice(Some("Player"), &["Buds", "TV"], &[])]
        );
        // Dismissed, it stays away while that output drops out for a moment.
        adapter.dismiss_all_notices();
        adapter.audio_mut().disconnect_output(BUDS);
        check_routing(&mut adapter);
        adapter
            .audio_mut()
            .connect_output_as(BUDS, "Buds", OutputKind::Bluetooth);
        check_routing(&mut adapter);
        assert!(heard_twice(&adapter).is_empty(), "after the drop-out");
        // The app quits and starts again: a notice about the new one.
        adapter.audio_mut().quit_app(PLAYER_PID);
        adapter.poll();
        assert!(!adapter.snapshot().running);
        adapter.audio_mut().add_app(2100, "player.exe");
        adapter.audio_mut().play_on(2100, TV);
        adapter.set_running(true);
        assert!(adapter.snapshot().running);
        assert_eq!(heard_twice(&adapter), [twice(Some("Player"), &["TV"], &[])]);
    }

    const PASSWORD_TXT: &[&str] = &[
        "txtvers=1",
        "ch=2",
        "cn=0,1",
        "et=0",
        "sr=44100",
        "ss=16",
        "tp=UDP",
        "pw=true",
    ];

    /// [`adapter`] with an AirPlay sender that lists Kitchen, which can
    /// play, and Study, which asks for a password.
    fn airplay_adapter() -> (AudioAdapter<MockBackend>, FakeAirPlay, Device, Device) {
        let mut audio = MockBackend::empty();
        audio.connect_output(SPEAKERS, "Speakers");
        audio.connect_output(HEADPHONES, "Headphones");
        audio.add_app(PLAYER_PID, "player.exe");
        let fake = FakeAirPlay::new();
        audio.set_airplay(fake.link(), None);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        let study = device(2, "Study", PASSWORD_TXT);
        fake.appear(&kitchen);
        fake.appear(&study);
        let adapter = AudioAdapter::new(audio, || "09.41".to_owned(), DebugLog::default());
        (adapter, fake, kitchen, study)
    }

    /// The UI's ID of an AirPlay speaker.
    fn ap(device: &Device) -> String {
        format!("ap:{}", id_of(device))
    }

    fn local_delay(adapter: &AudioAdapter<MockBackend>, endpoint_id: &str) -> i64 {
        adapter
            .audio
            .playing_params(endpoint_id)
            .expect("it plays")
            .delay_frames
    }

    #[test]
    fn an_airplay_receiver_that_appears_becomes_a_connected_speaker_row() {
        let (mut adapter, fake, kitchen, study) = airplay_adapter();
        let state = adapter.snapshot();
        let row = state.speaker(&ap(&kitchen)).expect("Kitchen is listed");
        assert_eq!(row.name, "Kitchen");
        assert_eq!(row.kind, SpeakerKind::AirPlay);
        assert_eq!(row.delay_ms, None, "AirPlay rows have no sync delay");
        assert_eq!(row.availability, Availability::Open);
        assert!(!row.enabled);
        assert_eq!(
            state.speaker(&ap(&study)).unwrap().availability,
            Availability::PasswordNotYet
        );
        let (this_pc, connected) = state.layout();
        assert!(this_pc.is_empty());
        let names: Vec<&str> = connected.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Headphones", "Kitchen", "Speakers", "Study"]);

        let bedroom = device(3, "Bedroom", OPEN_TXT);
        fake.appear(&bedroom);
        assert!(adapter.poll(), "the snapshot changed");
        assert!(adapter.snapshot().speaker(&ap(&bedroom)).is_some());
        fake.gone(&bedroom);
        assert!(adapter.poll());
        assert!(adapter.snapshot().speaker(&ap(&bedroom)).is_none());
    }

    #[test]
    fn turning_an_airplay_speaker_on_and_off_lines_local_outputs_up_and_back() {
        let (mut adapter, fake, kitchen, _) = airplay_adapter();
        turn_on(&mut adapter, SPEAKERS);
        turn_on(&mut adapter, HEADPHONES);
        adapter.set_speaker_delay(&key(SPEAKERS), 150.0);
        assert!(adapter.snapshot().running);
        assert_eq!(local_delay(&adapter, SPEAKERS), 7_200, "150 ms after");
        fake.take_calls();

        adapter.set_speaker_enabled(&ap(&kitchen), true);
        assert!(adapter.snapshot().speaker(&ap(&kitchen)).unwrap().enabled);
        assert!(matches!(fake.take_calls()[..], [Call::Add { .. }]));
        assert_eq!(local_delay(&adapter, SPEAKERS), 91_200, "L + 150 ms");

        adapter.set_speaker_enabled(&ap(&kitchen), false);
        assert_eq!(
            local_delay(&adapter, SPEAKERS),
            7_200,
            "its own delay again"
        );
        assert!(adapter.snapshot().running, "Speakers is still on");
    }

    #[test]
    fn an_airplay_speaker_alone_starts_and_stops_streaming() {
        let (mut adapter, fake, kitchen, _) = airplay_adapter();
        adapter.set_speaker_enabled(&ap(&kitchen), true);
        assert!(adapter.snapshot().running, "one AirPlay speaker is enough");
        assert!(playing(&adapter).is_empty());
        assert_eq!(fake.take_calls()[0], Call::RestartTimeline);
        adapter.set_speaker_enabled(&ap(&kitchen), false);
        assert!(!adapter.snapshot().running, "nothing is on anymore");
    }

    /// `airplay_adapter()` with an Apple TV that asks for a PIN listed too.
    fn apple_tv_adapter() -> (AudioAdapter<MockBackend>, FakeAirPlay, Device) {
        let (mut adapter, fake, _, _) = airplay_adapter();
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        fake.appear(&tv);
        adapter.poll();
        fake.take_calls();
        (adapter, fake, tv)
    }

    #[test]
    fn switching_on_an_apple_tv_pairs_it_and_then_it_streams() {
        let (mut adapter, fake, tv) = apple_tv_adapter();
        let id = ap(&tv);
        assert_eq!(
            adapter.snapshot().speaker(&id).unwrap().availability,
            Availability::NeedsCode
        );
        adapter.set_speaker_enabled(&id, true);
        assert_eq!(fake.take_calls(), [Call::StartPairing(id_of(&tv))]);
        let state = adapter.snapshot();
        assert!(!state.running, "nothing streams while it pairs");
        assert_eq!(state.pin_prompt, None, "no prompt until the PIN shows");
        assert_eq!(
            state.speaker(&id).unwrap().availability,
            Availability::Pairing
        );

        fake.pairing(&tv, PairingEvent::PinRequested);
        assert!(adapter.poll());
        assert_eq!(
            adapter.snapshot().pin_prompt,
            Some(PinPrompt {
                speaker_id: id.clone(),
                name: "Den TV".to_owned(),
                phase: PinPhase::Ready,
                wrong_pin: false,
            })
        );
        adapter.submit_pin(&id, "4821");
        assert_eq!(
            fake.take_calls(),
            [Call::SubmitPin(id_of(&tv), "4821".to_owned())]
        );
        assert_eq!(
            adapter.snapshot().pin_prompt.unwrap().phase,
            PinPhase::Checking
        );

        fake.pairing(
            &tv,
            PairingEvent::Paired(credentials("0123456789ABCDEF", 0x11)),
        );
        assert!(adapter.poll());
        let state = adapter.snapshot();
        assert_eq!(state.pin_prompt, None);
        let row = state.speaker(&id).unwrap();
        assert!(row.enabled && row.paired);
        assert_eq!(row.availability, Availability::Open);
        assert!(state.running, "paired and on: streaming starts");
        assert!(
            fake.take_calls().contains(&Call::Credentials(
                id_of(&tv),
                "0123456789ABCDEF".to_owned()
            )),
            "it joins with its credentials"
        );

        adapter.forget_pairing(&id);
        let state = adapter.snapshot();
        let row = state.speaker(&id).unwrap();
        assert!(!row.enabled && !row.paired);
        assert_eq!(row.availability, Availability::NeedsCode);
        assert!(!state.running, "nothing else is on");
        assert_eq!(fake.take_calls().last(), Some(&Call::Remove(id_of(&tv))));
    }

    #[test]
    fn a_wrong_pin_keeps_the_prompt_and_cancel_leaves_the_speaker_off() {
        let (mut adapter, fake, tv) = apple_tv_adapter();
        let id = ap(&tv);
        adapter.set_speaker_enabled(&id, true);
        fake.pairing(&tv, PairingEvent::PinRequested);
        adapter.poll();
        adapter.submit_pin(&id, "1111");
        fake.take_calls();
        fake.pairing(
            &tv,
            PairingEvent::Failed {
                error: SessionError::new(ErrorKind::PairingFailed),
                wrong_pin: true,
            },
        );
        adapter.poll();
        assert_eq!(fake.take_calls(), [Call::StartPairing(id_of(&tv))]);
        let prompt = adapter.snapshot().pin_prompt.expect("the prompt stays");
        assert!(prompt.wrong_pin);
        assert_eq!(prompt.phase, PinPhase::Starting);
        assert!(messages(&adapter).is_empty(), "no notice for a wrong PIN");

        adapter.cancel_pairing(&id);
        assert_eq!(fake.take_calls(), [Call::CancelPairing(id_of(&tv))]);
        let state = adapter.snapshot();
        assert_eq!(state.pin_prompt, None);
        assert!(!state.speaker(&id).unwrap().enabled);
        assert!(!state.running);
        fake.pairing(&tv, PairingEvent::Stopped);
        adapter.poll();
        assert!(messages(&adapter).is_empty(), "no notice for a cancel");
    }

    #[test]
    fn a_failed_pairing_closes_the_prompt_with_a_notice() {
        let (mut adapter, fake, tv) = apple_tv_adapter();
        let id = ap(&tv);
        adapter.set_speaker_enabled(&id, true);
        fake.pairing(&tv, PairingEvent::PinRequested);
        adapter.poll();
        fake.pairing(
            &tv,
            PairingEvent::Failed {
                error: SessionError::new(ErrorKind::Unreachable),
                wrong_pin: false,
            },
        );
        adapter.poll();
        let state = adapter.snapshot();
        assert_eq!(state.pin_prompt, None);
        assert!(!state.speaker(&id).unwrap().enabled);
        assert_eq!(
            messages(&adapter),
            [NoticeMessage::AirPlayCantPlay {
                speaker: "Den TV".to_owned(),
                problem: ui_model::AirPlayProblem::PairingFailed,
            }]
        );
    }

    #[test]
    fn auto_connect_never_starts_a_pairing() {
        let (mut adapter, fake, tv) = apple_tv_adapter();
        let id = ap(&tv);
        adapter.set_auto_connect(&id, true);
        fake.gone(&tv);
        adapter.poll();
        fake.appear(&tv);
        adapter.poll();
        assert!(
            !fake
                .take_calls()
                .iter()
                .any(|call| matches!(call, Call::StartPairing(_))),
            "no PIN appears on the TV by itself"
        );
        let state = adapter.snapshot();
        assert_eq!(state.pin_prompt, None);
        assert!(!state.speaker(&id).unwrap().enabled);
        assert_eq!(
            messages(&adapter),
            [NoticeMessage::AirPlayCantPlay {
                speaker: "Den TV".to_owned(),
                problem: ui_model::AirPlayProblem::NeedsPin,
            }]
        );
    }

    #[test]
    fn an_airplay_speaker_that_cant_play_stays_off_with_a_notice() {
        let (mut adapter, fake, _, study) = airplay_adapter();
        adapter.set_speaker_enabled(&ap(&study), true);
        assert!(!adapter.snapshot().speaker(&ap(&study)).unwrap().enabled);
        assert!(!adapter.snapshot().running);
        assert!(fake.take_calls().is_empty(), "never connected");
        assert_eq!(
            messages(&adapter),
            [NoticeMessage::AirPlayCantPlay {
                speaker: "Study".to_owned(),
                problem: ui_model::AirPlayProblem::PasswordNotYet,
            }]
        );
    }

    #[test]
    fn airplay_errors_become_notices() {
        let (mut adapter, fake, kitchen, _) = airplay_adapter();
        adapter.set_speaker_enabled(&ap(&kitchen), true);
        fake.state(&kitchen, AirPlayState::Playing);
        adapter.poll();
        fake.fail(&kitchen, ErrorKind::Unreachable);
        assert!(adapter.poll());
        assert_eq!(
            messages(&adapter),
            [NoticeMessage::AirPlayRetrying {
                speaker: "Kitchen".to_owned(),
                problem: ui_model::AirPlayProblem::NotResponding,
            }]
        );
        assert!(adapter.snapshot().speaker(&ap(&kitchen)).unwrap().enabled);
        // It plays again: the notice goes.
        fake.state(&kitchen, AirPlayState::Playing);
        adapter.poll();
        assert!(messages(&adapter).is_empty());
        // One that needs the user is turned off, and streaming stops with it.
        fake.fail(&kitchen, ErrorKind::PairingRequired);
        adapter.poll();
        assert_eq!(
            messages(&adapter),
            [NoticeMessage::AirPlayCantPlay {
                speaker: "Kitchen".to_owned(),
                problem: ui_model::AirPlayProblem::PairingNotYet,
            }]
        );
        assert!(!adapter.snapshot().speaker(&ap(&kitchen)).unwrap().enabled);
        assert!(!adapter.snapshot().running);
    }

    #[test]
    fn a_source_switch_keeps_airplay_speakers_in_the_stream() {
        let (mut adapter, fake, kitchen, _) = airplay_adapter();
        adapter.set_speaker_enabled(&ap(&kitchen), true);
        assert!(adapter.snapshot().running);
        fake.take_calls();
        adapter.select_source(&player());
        assert_eq!(
            captures(&adapter)[1],
            (LoopbackMode::IncludeTree, PLAYER_PID),
            "capture starts again from the player"
        );
        assert_eq!(
            fake.take_calls(),
            [Call::RestartTimeline],
            "the speaker stays in the stream, only its timeline restarts (S141)"
        );
        assert!(adapter.snapshot().running);
    }

    #[test]
    fn groups_and_auto_connect_hold_airplay_speakers() {
        let (mut adapter, fake, kitchen, _) = airplay_adapter();
        turn_on(&mut adapter, SPEAKERS);
        adapter.set_speaker_enabled(&ap(&kitchen), true);
        adapter.new_group("Downstairs").unwrap();
        let group = adapter.snapshot().groups[0].clone();
        assert_eq!(group.members, [key(SPEAKERS), ap(&kitchen)]);
        adapter.toggle_group(&group.id);
        assert!(!adapter.snapshot().running, "both switched off");
        assert!(!adapter.snapshot().speaker(&ap(&kitchen)).unwrap().enabled);

        // Auto-connect: Kitchen switches on when it appears again.
        adapter.set_auto_connect(&ap(&kitchen), true);
        fake.gone(&kitchen);
        adapter.poll();
        fake.appear(&kitchen);
        adapter.poll();
        assert!(adapter.snapshot().speaker(&ap(&kitchen)).unwrap().enabled);
        assert!(adapter.snapshot().running);
    }

    /// The mock world at a launch: both outputs and the player app.
    fn launch_audio() -> MockBackend {
        let mut audio = MockBackend::empty();
        audio.connect_output(SPEAKERS, "Speakers");
        audio.connect_output(HEADPHONES, "Headphones");
        audio.add_app(PLAYER_PID, "player.exe");
        audio
    }

    /// The adapter at a launch over `audio`, with the settings in `file`.
    fn launch(audio: MockBackend, file: &std::path::Path) -> AudioAdapter<MockBackend> {
        let file = SettingsFile::at(file.to_path_buf());
        AudioAdapter::with_settings(audio, || "09.41".to_owned(), DebugLog::default(), file)
    }

    fn speaker_of(adapter: &AudioAdapter<MockBackend>, id: &str) -> Speaker {
        adapter.snapshot().speaker(id).cloned().expect("listed")
    }

    #[test]
    fn the_users_choices_survive_a_restart_without_streaming() {
        let dir = test_dir("adapter-settings-restart");
        let path = dir.join(settings::FILE_NAME);
        let mut adapter = launch(launch_audio(), &path);
        adapter.poll();
        assert!(!path.exists(), "nothing changed, nothing written");

        adapter.select_source(&player());
        adapter.set_master_volume(70.0);
        turn_on(&mut adapter, SPEAKERS);
        adapter.set_speaker_volume(&key(SPEAKERS), 35.0);
        adapter.set_speaker_delay(&key(SPEAKERS), 240.0);
        adapter.set_speaker_delay(&key(HEADPHONES), 15.0);
        adapter.new_group("Desk").expect("made");
        let mut equalizer = EqSettings {
            enabled: true,
            ..EqSettings::default()
        };
        equalizer.gains_db[0] = 4.5;
        adapter.set_equalizer(equalizer);
        adapter.set_view_settings(ViewSettings {
            show_virtual_devices: true,
            window: None,
        });
        assert!(adapter.snapshot().running);
        adapter.poll();
        let groups = adapter.snapshot().groups;

        // The app quits while streaming, and starts again with the player
        // running under a new process ID.
        let mut audio = launch_audio();
        audio.quit_app(PLAYER_PID);
        audio.add_app(PLAYER_PID + 7, "player.exe");
        let mut adapter = launch(audio, &path);
        let state = adapter.snapshot();
        assert!(!state.running, "a launch doesn't start streaming by itself");
        assert_eq!(
            state.source_id,
            format!("app:{}:player.exe", PLAYER_PID + 7)
        );
        assert_eq!(state.master_volume, 70.0);
        let speakers = speaker_of(&adapter, &key(SPEAKERS));
        assert!(speakers.enabled);
        assert_eq!(speakers.volume, 35.0);
        assert_eq!(speakers.delay_ms, Some(240.0));
        let headphones = speaker_of(&adapter, &key(HEADPHONES));
        assert!(!headphones.enabled);
        assert_eq!(headphones.delay_ms, Some(15.0));
        assert_eq!(state.groups, groups);
        assert_eq!(state.equalizer, equalizer);
        assert!(adapter.view_settings().show_virtual_devices);

        // A new group gets the next number, and streaming starts from the
        // saved speakers when the user asks.
        adapter.new_group("Two").expect("made");
        assert_eq!(adapter.snapshot().groups[1].id, "group:2");
        adapter.set_running(true);
        assert_eq!(playing(&adapter), [SPEAKERS]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The PC's own speakers (the reference), a Bluetooth speaker and a TV,
    /// as at a launch.
    fn reference_audio() -> MockBackend {
        let mut audio = MockBackend::empty();
        audio.connect_output_as("builtin", "Speakers", OutputKind::BuiltIn);
        audio.connect_output_as("bluetooth", "Earbuds", OutputKind::Bluetooth);
        audio.connect_output_as("tv", "TV", OutputKind::Hdmi);
        audio
    }

    #[test]
    fn an_older_settings_file_is_migrated_once_and_saved_in_the_new_version() {
        let dir = test_dir("adapter-settings-delay-migration");
        let path = dir.join(settings::FILE_NAME);
        std::fs::write(
            &path,
            r#"{
                "version": 1,
                "outputs": {
                    "builtin": {"enabled": true, "delay_ms": 250},
                    "bluetooth": {"enabled": true, "delay_ms": 0},
                    "tv": {"delay_ms": 120}
                }
            }"#,
        )
        .expect("written");
        let adapter = launch(reference_audio(), &path);
        let state = adapter.snapshot();
        assert_eq!(state.reference_name.as_deref(), Some("Speakers"));
        assert_eq!(speaker_of(&adapter, &key("builtin")).delay_ms, None);
        assert_eq!(
            speaker_of(&adapter, &key("bluetooth")).delay_ms,
            Some(-250.0)
        );
        assert_eq!(speaker_of(&adapter, &key("tv")).delay_ms, Some(-130.0));
        assert_eq!(adapter.audio.airplay_adjustment(), 250.0);
        let (saved, found) = SettingsFile::at(path.clone()).load();
        assert_eq!(
            found,
            settings::Loaded::Read {
                version: settings::VERSION,
                adjusted: false
            }
        );
        assert_eq!(settings::VERSION, 2);
        assert_eq!(saved.outputs["bluetooth"].delay_ms, -250.0);
        assert_eq!(saved.outputs["tv"].delay_ms, -130.0);
        assert_eq!(saved.outputs["builtin"].delay_ms, 0.0);
        assert!(saved.outputs["builtin"].enabled);
        assert_eq!(saved.airplay_adjustment_ms, 250.0);

        // The next launch reads the new format and migrates nothing again.
        let adapter = launch(reference_audio(), &path);
        assert_eq!(
            speaker_of(&adapter, &key("bluetooth")).delay_ms,
            Some(-250.0)
        );
        assert_eq!(adapter.audio.airplay_adjustment(), 250.0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_older_file_waits_for_an_output_list_before_it_is_migrated() {
        let dir = test_dir("adapter-settings-delay-migration-wait");
        let path = dir.join(settings::FILE_NAME);
        std::fs::write(
            &path,
            r#"{"version": 1, "outputs": {"builtin": {"delay_ms": 250}, "tv": {"delay_ms": 120}}}"#,
        )
        .expect("written");
        let mut audio = reference_audio();
        audio.fail_listing(Some(BackendError::System {
            call: "IMMDeviceEnumerator::EnumAudioEndpoints",
            code: -1,
        }));
        let mut adapter = launch(audio, &path);
        adapter.poll();
        let (saved, found) = SettingsFile::at(path.clone()).load();
        assert!(
            matches!(found, settings::Loaded::Read { version: 1, .. }),
            "not saved before the migration: {found}"
        );
        assert_eq!(saved.outputs["tv"].delay_ms, 120.0);
        adapter.audio_mut().fail_listing(None);
        adapter.poll();
        assert_eq!(speaker_of(&adapter, &key("tv")).delay_ms, Some(-130.0));
        let (saved, _) = SettingsFile::at(path.clone()).load();
        assert_eq!(saved.outputs["tv"].delay_ms, -130.0);
        assert_eq!(saved.airplay_adjustment_ms, 250.0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_airplay_delay_field_shows_l_plus_a_and_follows_a_new_l() {
        let (mut with_sender, fake, _, _) = airplay_adapter();
        let shown = |adapter: &AudioAdapter<MockBackend>| adapter.snapshot().airplay_delay_ms;
        assert_eq!(shown(&with_sender), Some(1750.0), "L_s + 0");
        with_sender.set_airplay_delay(2000.0);
        assert_eq!(shown(&with_sender), Some(2000.0));
        assert_eq!(with_sender.audio.airplay_adjustment(), 250.0);
        fake.set_latency(std::time::Duration::from_secs(1));
        with_sender.poll();
        assert_eq!(shown(&with_sender), Some(1250.0));
        with_sender.set_airplay_delay(f64::NAN);
        assert_eq!(shown(&with_sender), Some(1250.0));
        with_sender.set_airplay_delay(-500.0);
        assert_eq!(shown(&with_sender), Some(0.0), "clamped");
        // Without an AirPlay sender there is no field.
        assert_eq!(shown(&adapter()), None);
    }

    #[test]
    fn an_auto_connect_speaker_that_is_there_at_launch_starts_streaming() {
        let dir = test_dir("adapter-settings-auto-connect");
        let path = dir.join(settings::FILE_NAME);
        let mut adapter = launch(launch_audio(), &path);
        adapter.set_auto_connect(&key(HEADPHONES), true);
        adapter.save_settings();

        let adapter = launch(launch_audio(), &path);
        assert!(adapter.snapshot().running);
        assert_eq!(playing(&adapter), [HEADPHONES]);

        // Not there at launch: nothing starts until it is plugged in.
        let mut audio = launch_audio();
        audio.disconnect_output(HEADPHONES);
        let mut adapter = launch(audio, &path);
        assert!(!adapter.snapshot().running);
        adapter.audio_mut().connect_output(HEADPHONES, "Headphones");
        adapter.poll();
        assert_eq!(playing(&adapter), [HEADPHONES]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_device_missing_at_launch_keeps_its_settings_until_it_is_back() {
        let dir = test_dir("adapter-settings-missing");
        let path = dir.join(settings::FILE_NAME);
        let mut adapter = launch(launch_audio(), &path);
        turn_on(&mut adapter, HEADPHONES);
        adapter.set_speaker_delay(&key(HEADPHONES), 120.0);
        adapter.set_speaker_volume(&key(HEADPHONES), 25.0);
        adapter.set_running(false);
        adapter.poll();

        let mut audio = launch_audio();
        audio.disconnect_output(HEADPHONES);
        let mut adapter = launch(audio, &path);
        assert!(adapter.snapshot().speaker(&key(HEADPHONES)).is_none());
        // Other changes are saved; the missing device's settings stay.
        adapter.set_speaker_volume(&key(SPEAKERS), 80.0);
        adapter.poll();
        let (saved, _) = SettingsFile::at(path.clone()).load();
        assert_eq!(
            saved.outputs.get(HEADPHONES),
            Some(&SavedSpeaker {
                enabled: true,
                volume: 25.0,
                delay_ms: 120.0
            })
        );
        assert_eq!(saved.outputs[SPEAKERS].volume, 80.0);

        adapter.audio_mut().connect_output(HEADPHONES, "Headphones");
        adapter.poll();
        let headphones = speaker_of(&adapter, &key(HEADPHONES));
        assert!(headphones.enabled);
        assert_eq!(headphones.volume, 25.0);
        assert_eq!(headphones.delay_ms, Some(120.0));
        assert!(
            !adapter.snapshot().running,
            "it isn't an auto-connect speaker"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn airplay_speakers_get_their_settings_back_when_found() {
        let dir = test_dir("adapter-settings-airplay");
        let path = dir.join(settings::FILE_NAME);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        let with_airplay = |fake: &FakeAirPlay| {
            let mut audio = launch_audio();
            audio.set_airplay(fake.link(), None);
            audio
        };
        let fake = FakeAirPlay::new();
        fake.appear(&kitchen);
        let mut adapter = launch(with_airplay(&fake), &path);
        adapter.set_speaker_enabled(&ap(&kitchen), true);
        adapter.set_speaker_volume(&ap(&kitchen), 30.0);
        adapter.set_running(false);
        adapter.poll();

        // Found only after the launch, as discovery does.
        let fake = FakeAirPlay::new();
        let mut adapter = launch(with_airplay(&fake), &path);
        assert!(adapter.snapshot().speaker(&ap(&kitchen)).is_none());
        fake.appear(&kitchen);
        adapter.poll();
        let speaker = speaker_of(&adapter, &ap(&kitchen));
        assert!(speaker.enabled);
        assert_eq!(speaker.volume, 30.0);

        // Without an AirPlay sender the speaker stays off, and its settings
        // stay saved for a launch that has one.
        let mut adapter = launch(launch_audio(), &path);
        adapter.set_master_volume(50.0);
        adapter.poll();
        let (saved, _) = SettingsFile::at(path.clone()).load();
        assert_eq!(saved.master_volume, 50.0);
        assert_eq!(
            saved.airplay.get(&id_of(&kitchen)),
            Some(&SavedSpeaker {
                enabled: true,
                volume: 30.0,
                delay_ms: 0.0
            })
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_bad_settings_file_is_logged_and_ignored_then_replaced() {
        let dir = test_dir("adapter-settings-log");
        let path = dir.join(settings::FILE_NAME);
        std::fs::write(&path, "{\"version\": 1, \"outputs\": ").expect("written");
        let log = DebugLog::new(Some(dir.join(FILE_NAME)));
        let file = SettingsFile::at(path.clone());
        let mut adapter =
            AudioAdapter::with_settings(launch_audio(), || "09.41".to_owned(), log.clone(), file);
        assert!(!adapter.snapshot().running);
        assert_eq!(
            speaker_of(&adapter, &key(SPEAKERS)).volume,
            DEFAULT_LOCAL_POSITION
        );
        adapter.poll();
        assert!(
            std::fs::read_to_string(&path)
                .expect("still there")
                .ends_with(": "),
            "an ignored file is left alone until something changes"
        );
        adapter.set_master_volume(64.0);
        adapter.poll();
        assert_eq!(SettingsFile::at(path.clone()).load().0.master_volume, 64.0);

        let file = SettingsFile::at(path.clone());
        let _adapter =
            AudioAdapter::with_settings(launch_audio(), || "09.41".to_owned(), log.clone(), file);
        assert!(log.flush(TEST_FLUSH));
        assert_in_order(
            &read_messages(&dir.join(FILE_NAME)),
            &[
                "settings: ignored: it isn't valid (",
                "settings: saved",
                "settings: read (format version 2)",
            ],
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_newer_settings_file_is_kept_aside_once_and_logged() {
        let dir = test_dir("adapter-settings-newer");
        let path = dir.join(settings::FILE_NAME);
        let newer = format!("{{\"version\": {}}}", settings::VERSION + 1);
        std::fs::write(&path, &newer).expect("written");
        let log = DebugLog::new(Some(dir.join(FILE_NAME)));
        let file = SettingsFile::at(path.clone());
        let mut adapter =
            AudioAdapter::with_settings(launch_audio(), || "09.41".to_owned(), log.clone(), file);
        adapter.set_master_volume(64.0);
        adapter.poll();
        adapter.set_master_volume(65.0);
        adapter.poll();
        let kept = dir.join(settings::NEWER_FILE_NAME);
        assert_eq!(std::fs::read_to_string(kept).expect("kept"), newer);
        assert!(log.flush(TEST_FLUSH));
        let messages = read_messages(&dir.join(FILE_NAME));
        assert_in_order(
            &messages,
            &[
                "settings: ignored: a newer AudioSync wrote it",
                "settings: a newer AudioSync's file was kept as settings.newer.json",
                "settings: saved",
                "settings: saved",
            ],
        );
        let kept_lines = messages
            .iter()
            .filter(|message| message.contains("was kept as"))
            .count();
        assert_eq!(kept_lines, 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
