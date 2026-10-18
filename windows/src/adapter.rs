//! The UI's [`Backend`] over the audio backend.
//!
//! The windows show what the audio backend lists, the sources to capture and
//! the connected outputs with their settings, and every user action becomes
//! audio backend calls, decided by the session rules of `audiosync-core`
//! (`policy::session`): switching a speaker on starts streaming, switching the
//! last one off stops it, an output that can't start is turned off, a source
//! that quits ends the session. What happens on its own comes from
//! [`AudioBackend::poll`], which [`Backend::poll`] passes on, and the adapter
//! turns it into notices. Notices carry a [`NoticeMessage`]; the UI words it.
//!
//! The chosen source, groups, auto-connect marks and notices live here, not in
//! the audio backend. Like the speakers' settings they are kept in memory only
//! for now: AudioSync for Windows doesn't save its settings yet.
//!
//! Speakers are named by the core's speaker keys (`lc:` and the endpoint ID),
//! so groups and auto-connect marks can hold AirPlay speakers (`ap:`) once
//! Windows has them. Sources are `system`, or `app:` with the app's process ID
//! and executable (`app:2000:spotify.exe`), so a process ID that Windows gives
//! to another app later is never taken for the chosen one.
//!
//! The debug log gets what the user asked for, what the session rules
//! decided, the notices posted and the source list whenever it changes; the
//! audio backend logs what it did itself.

use std::collections::{BTreeMap, BTreeSet};

use audiosync_core::eq::EqSettings;
use audiosync_core::policy::session::{
    AutoStartGate, Incident, SpeakerEvent, StartOrigin, StreamingAction, may_start, reaction,
    source_switch, streaming_after,
};
use audiosync_core::policy::speakers::{NoticeSubject, Notices, SpeakerKey, renamed_group};

use crate::audio::backend::{
    AudioBackend, BackendError, BackendEvent, OutputInfo, OutputKind, SourceId, SourceInfo,
    StopReason,
};
use crate::audio::capture::WHOLE_SYSTEM_NAME;
use crate::debug_log::DebugLog;
use crate::ui_model::{
    AppState, Availability, Backend, Group, GroupError, Notice, NoticeMessage, Source, SourceKind,
    Speaker, SpeakerKind, clamp_delay_ms,
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
    /// The speaker keys marked auto-connect.
    auto_connect: BTreeSet<String>,
    groups: Vec<Group>,
    next_group: u64,
    gate: AutoStartGate,
    notices: Notices<NoticeSubject, Posted>,
    next_notice: u64,
    equalizer: EqSettings,
    /// Polls since the sources were last listed.
    polls: u32,
    /// The local time now, as a notice shows it.
    clock: fn() -> String,
    /// The debug log; it writes nothing unless `AUDIOSYNC_DEBUG=1`.
    log: DebugLog,
}

impl<A: AudioBackend> AudioAdapter<A> {
    /// The adapter over `audio`, stopped, with the sources and outputs listed
    /// now. `clock` gives the time a notice shows; what happens goes to
    /// `log`.
    pub fn new(audio: A, clock: fn() -> String, log: DebugLog) -> AudioAdapter<A> {
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
            auto_connect: BTreeSet::new(),
            groups: Vec::new(),
            next_group: 1,
            gate: AutoStartGate::default(),
            notices: Notices::new(),
            next_notice: 1,
            equalizer: EqSettings::default(),
            polls: 0,
            clock,
            log,
        };
        adapter.list_devices();
        adapter
            .log
            .log(|| format!("source chosen: {WHOLE_SYSTEM_NAME} (the default)"));
        adapter
    }

    /// The audio backend, to change the mock's pretend world in tests.
    #[cfg(test)]
    pub fn audio_mut(&mut self) -> &mut A {
        &mut self.audio
    }

    fn running(&self) -> bool {
        self.audio.running_source().is_some()
    }

    /// Outputs turned on, connected or not.
    fn enabled_count(&self) -> usize {
        self.audio.enabled_outputs().len()
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
        let _ = self.audio.list_outputs();
        self.take_events();
        self.outputs_listed();
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
    fn start_session(&mut self, origin: StartOrigin) {
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
            return;
        }
        if !may_start(self.enabled_count()) {
            self.log
                .log(|| format!("start ({origin:?}) refused: no speaker is on"));
            self.post(NoticeSubject::Start, NoticeMessage::NoSpeaker);
            return;
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
            }
            Err(error) => {
                self.log
                    .log(|| format!("start ({origin:?}) failed: {error}"));
                self.gate.start_failed(origin);
                self.audio.stop();
                let message = self.start_problem(&error);
                self.post(NoticeSubject::Start, message);
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

    /// Stops streaming when a speaker was switched off and none is left on
    /// (S118, S162).
    fn stop_if_idle(&mut self) {
        let action = streaming_after(
            SpeakerEvent::SwitchedOff,
            self.running(),
            self.enabled_count(),
            0,
        );
        if action == StreamingAction::Stop {
            self.log
                .log(|| "streaming stops: no speaker is on anymore".to_owned());
            self.audio.stop();
        }
    }

    /// Starts streaming when a speaker was switched on while nothing streams
    /// (S119, S162).
    fn start_if_switched_on(&mut self) {
        let action = streaming_after(
            SpeakerEvent::SwitchedOn,
            self.running(),
            self.enabled_count(),
            0,
        );
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
            BackendEvent::OutputsChanged => self.outputs_listed(),
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
                    StopReason::SourceQuit => NoticeMessage::SourceQuit {
                        source: self.chosen_name(),
                    },
                    StopReason::CaptureFailed(_) => NoticeMessage::CaptureLost,
                };
                self.post(NoticeSubject::Source, message);
                self.refresh_sources();
            }
        }
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

    fn speaker(&self, output: OutputInfo) -> Speaker {
        let id = speaker_key(&output.id);
        Speaker {
            auto_connect: self.auto_connect.contains(&id),
            id,
            name: output.name,
            kind: speaker_kind(output.kind),
            enabled: output.enabled,
            volume: output.volume,
            delay_ms: Some(output.delay_ms),
            availability: Availability::Open,
        }
    }
}

impl<A: AudioBackend> Backend for AudioAdapter<A> {
    fn snapshot(&self) -> AppState {
        AppState {
            running: self.running(),
            sources: self.source_rows(),
            source_id: self.chosen.key(),
            master_volume: self.audio.master_volume(),
            speakers: self
                .audio
                .outputs()
                .into_iter()
                .map(|output| self.speaker(output))
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
        let action = streaming_after(
            SpeakerEvent::StartRequested,
            self.running(),
            self.enabled_count(),
            0,
        );
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
        let switch = source_switch(self.running());
        if switch.stop_keeping_airplay {
            self.audio.stop();
        }
        if let Some(origin) = switch.start {
            self.start_session(origin);
        }
    }

    fn set_master_volume(&mut self, position: f64) {
        self.audio.set_master_volume(position);
    }

    fn set_speaker_enabled(&mut self, id: &str, on: bool) {
        let Some(endpoint_id) = local_id(id) else {
            return;
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
        if let Some(endpoint_id) = local_id(id) {
            self.audio.set_output_volume(&endpoint_id, position);
        }
    }

    fn set_speaker_delay(&mut self, id: &str, ms: f64) {
        if let Some(endpoint_id) = local_id(id) {
            self.audio
                .set_output_delay(&endpoint_id, clamp_delay_ms(ms));
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
        let members: Vec<String> = state
            .available_members(group)
            .filter_map(|speaker| local_id(&speaker.id))
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
        for endpoint_id in &members {
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
            self.notices.clear(&[subject]);
        }
    }

    fn dismiss_all_notices(&mut self) {
        self.notices.clear_all();
    }

    fn refresh_devices(&mut self) {
        self.log
            .log(|| "the user asked to list the devices again".to_owned());
        self.list_devices();
    }

    fn poll(&mut self) -> bool {
        let mut changed = self.take_events();
        self.polls += 1;
        if self.polls >= SOURCE_POLLS {
            self.polls = 0;
            changed |= self.refresh_sources();
        }
        changed
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

/// The endpoint ID of a local output's speaker key.
fn local_id(key: &str) -> Option<String> {
    match SpeakerKey::parse(key)? {
        SpeakerKey::Local(endpoint_id) => Some(endpoint_id),
        SpeakerKey::AirPlay(_) => None,
    }
}

/// What a notice about one local output is about.
fn speaker_subject(endpoint_id: &str) -> NoticeSubject {
    NoticeSubject::Speaker(SpeakerKey::Local(endpoint_id.to_owned()))
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

    use super::*;
    use crate::audio::capture::LoopbackMode;
    use crate::audio::mock::{MOCK_OWN_PID, MockBackend};
    use crate::debug_log::{FILE_NAME, TEST_FLUSH, assert_in_order, read_messages, test_dir};
    use crate::ui_model::{MAX_DELAY_MS, SpeakerState};

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
        adapter.set_speaker_volume(&key(SPEAKERS), 30.0);
        adapter.set_speaker_delay(&key(SPEAKERS), 120.4);
        adapter.set_master_volume(50.0);
        let state = adapter.snapshot();
        let speaker = state.speaker(&key(SPEAKERS)).unwrap();
        assert_eq!(speaker.volume, 30.0);
        assert_eq!(speaker.delay_ms, Some(120.0), "whole milliseconds");
        assert_eq!(state.master_volume, 50.0);
        let params = adapter.audio.playing_params(SPEAKERS).unwrap();
        assert_eq!(params.delay_frames, 5_760, "120 ms at 48 kHz");
        assert_eq!(params.gain, output_gain(30.0, 50.0) as f32);

        adapter.set_speaker_delay(&key(HEADPHONES), 99_999.0);
        let state = adapter.snapshot();
        assert_eq!(
            state.speaker(&key(HEADPHONES)).unwrap().delay_ms,
            Some(MAX_DELAY_MS),
            "a delay set while off is kept, within the limit"
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
}
