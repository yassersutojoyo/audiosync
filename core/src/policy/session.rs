//! Starting and stopping sessions, keeping their local outputs in sync, and the
//! host's reaction to everything that can go wrong (S20, S21, S68, S69, S116 to
//! S122, S136, S141, S142).

use super::pairing::DropReason;
use super::speakers::{NoticeSubject, SpeakerKey};

/// Who started a session (S120).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StartOrigin {
    /// The user: a click on a speaker, a source switch, Retry.
    User,
    /// Auto-connect.
    AutoConnect,
    /// The automatic restart after a capture format change (S20).
    FormatChange,
}

impl StartOrigin {
    /// Auto-connect and format-change restarts are automatic (S120).
    pub fn is_automatic(self) -> bool {
        !matches!(self, StartOrigin::User)
    }
}

/// Whether a session may start: only with at least one speaker, local or
/// AirPlay, enabled (S116). Otherwise the host posts an informational "start"
/// notice ([`Incident::NoSpeakerEnabled`]).
pub fn may_start(enabled_speakers: usize) -> bool {
    enabled_speakers > 0
}

/// One step the host takes after a successful start (S116).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterStart {
    /// Select the enabled AirPlay speakers in the engine.
    ApplyAirPlaySelection,
    /// Send the master volume to the engine (S72).
    SendMasterVolume,
    /// Add every enabled, present local output (S117).
    AddLocalOutputs,
    /// Ask the engine to play or stop the pipe (S99).
    EnsurePipePlayback,
    /// Start the capture check (S18).
    StartCaptureCheck,
}

/// The steps after a successful start, in order (S116).
pub const AFTER_START: [AfterStart; 5] = [
    AfterStart::ApplyAirPlaySelection,
    AfterStart::SendMasterVolume,
    AfterStart::AddLocalOutputs,
    AfterStart::EnsurePipePlayback,
    AfterStart::StartCaptureCheck,
];

/// The local outputs to add and remove so that the running renderers match the
/// enabled set (S117).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputSync {
    /// Enabled and present UIDs without a renderer, in enabled order. Each is
    /// added with its effective delay and gain.
    pub add: Vec<String>,
    /// UIDs with a renderer that are no longer enabled.
    pub remove: Vec<String>,
}

impl OutputSync {
    /// Whether nothing needs to change.
    pub fn is_empty(&self) -> bool {
        self.add.is_empty() && self.remove.is_empty()
    }
}

/// Output sync (S117), idempotent: while running, add every enabled, present UID
/// that has no renderer and remove every renderer whose UID is no longer
/// enabled; an enabled UID that isn't present waits for a later sync. Outputs
/// are added only while running.
///
/// Renderers are keyed by device UID, so a device that disappeared (its
/// renderer stopped, its UID still enabled) is added again once it is listed
/// again (S68), and a change of the system's default output changes nothing
/// (S145).
pub fn sync_outputs<E: AsRef<str>, P: AsRef<str>, A: AsRef<str>>(
    running: bool,
    enabled: &[E],
    present: &[P],
    active: &[A],
) -> OutputSync {
    let contains = |list: &[A], uid: &str| list.iter().any(|item| item.as_ref() == uid);
    let mut sync = OutputSync::default();
    if running {
        for uid in enabled.iter().map(AsRef::as_ref) {
            let is_present = present.iter().any(|item| item.as_ref() == uid);
            let added = sync.add.iter().any(|item| item == uid);
            if is_present && !contains(active, uid) && !added {
                sync.add.push(uid.to_owned());
            }
        }
    }
    for uid in active.iter().map(AsRef::as_ref) {
        let is_enabled = enabled.iter().any(|item| item.as_ref() == uid);
        if !is_enabled && !sync.remove.iter().any(|item| item == uid) {
            sync.remove.push(uid.to_owned());
        }
    }
    sync
}

/// The kind of speaker the user switched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeakerKind {
    /// A local output device.
    Local,
    /// An AirPlay speaker of the engine.
    AirPlay,
}

/// What turning a speaker on does (S119).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnOn {
    /// Not running: start a session with user origin.
    StartSession,
    /// Running, local speaker: sync the outputs in (S117).
    SyncLocalOutputs,
    /// Running, AirPlay speaker: select it in the engine.
    SelectAirPlay,
}

/// Turning a speaker on (S119).
pub fn turn_on(kind: SpeakerKind, running: bool) -> TurnOn {
    match (running, kind) {
        (false, _) => TurnOn::StartSession,
        (true, SpeakerKind::Local) => TurnOn::SyncLocalOutputs,
        (true, SpeakerKind::AirPlay) => TurnOn::SelectAirPlay,
    }
}

/// What turning a speaker off does (S118).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnOff {
    /// It was the last enabled speaker: stop the session and deselect every
    /// AirPlay speaker.
    StopSession,
    /// Remove its renderer (S117).
    SyncLocalOutputs,
    /// Deselect it in the engine.
    DeselectAirPlay,
    /// Not running: only the enabled set changes.
    Nothing,
}

/// Turning a speaker off, with `enabled_left` speakers (local and AirPlay)
/// still enabled afterwards (S118).
pub fn turn_off(kind: SpeakerKind, running: bool, enabled_left: usize) -> TurnOff {
    match (running, enabled_left, kind) {
        (false, _, _) => TurnOff::Nothing,
        (true, 0, _) => TurnOff::StopSession,
        (true, _, SpeakerKind::Local) => TurnOff::SyncLocalOutputs,
        (true, _, SpeakerKind::AirPlay) => TurnOff::DeselectAirPlay,
    }
}

/// How a speaker shows in the lists (S122).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeakerState {
    /// Enabled while streaming.
    Playing,
    /// Enabled while not streaming.
    WillPlay,
    /// Not enabled.
    Off,
}

/// A speaker's state (S122).
pub fn speaker_state(enabled: bool, streaming: bool) -> SpeakerState {
    match (enabled, streaming) {
        (false, _) => SpeakerState::Off,
        (true, true) => SpeakerState::Playing,
        (true, false) => SpeakerState::WillPlay,
    }
}

/// What a click on a speaker does (S122).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Click {
    /// Turn it off (S118).
    TurnOff,
    /// Turn it on, starting streaming when not streaming (S119).
    TurnOn,
}

/// A click on a speaker in this state (S122): a playing speaker turns off, a
/// waiting or off speaker turns on.
pub fn click(state: SpeakerState) -> Click {
    match state {
        SpeakerState::Playing => Click::TurnOff,
        SpeakerState::WillPlay | SpeakerState::Off => Click::TurnOn,
    }
}

/// Whether automatic starts are allowed (S120): a failed automatic start
/// suppresses further automatic starts until the user acts; switching streaming
/// off by hand suppresses auto-connect starts while speakers stay enabled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AutoStartGate {
    automatic_start_failed: bool,
    switched_off_by_user: bool,
}

impl AutoStartGate {
    /// Whether a start of this origin may run now.
    pub fn allows(&self, origin: StartOrigin) -> bool {
        match origin {
            StartOrigin::User => true,
            StartOrigin::FormatChange => !self.automatic_start_failed,
            StartOrigin::AutoConnect => !self.automatic_start_failed && !self.switched_off_by_user,
        }
    }

    /// A start of this origin failed.
    pub fn start_failed(&mut self, origin: StartOrigin) {
        if origin.is_automatic() {
            self.automatic_start_failed = true;
        }
    }

    /// The user acted: started streaming or switched a speaker on. Automatic
    /// starts are allowed again.
    pub fn user_acted(&mut self) {
        self.automatic_start_failed = false;
        self.switched_off_by_user = false;
    }

    /// The user switched streaming off by hand.
    pub fn user_switched_off(&mut self) {
        self.switched_off_by_user = true;
    }
}

/// What a source switch does (S141).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceSwitch {
    /// Stop the session without deselecting AirPlay speakers (deselecting would
    /// race the reselect); the engine's pipe playback stops with the session.
    pub stop_keeping_airplay: bool,
    /// Then start a session with this origin (ring reset, renderers added
    /// again, pipe playback requested again shortly after).
    pub start: Option<StartOrigin>,
}

/// A source switch (S141). While not running, the new source is simply used by
/// the next start.
pub fn source_switch(running: bool) -> SourceSwitch {
    SourceSwitch {
        stop_keeping_airplay: running,
        start: running.then_some(StartOrigin::User),
    }
}

/// Something that went wrong, or might have (S136).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Incident {
    /// A start was requested with no speaker enabled (S116).
    NoSpeakerEnabled,
    /// The microphone permission was denied for an input source (S15).
    MicrophoneDenied {
        /// Who asked for the start.
        origin: StartOrigin,
    },
    /// The engine's `start` threw (S121).
    StartFailed {
        /// Who asked for the start.
        origin: StartOrigin,
    },
    /// The capture check says audio is not arriving yet, or not at all (S18).
    NoAudio,
    /// `interrupted(formatChanged)` (S20).
    FormatChanged,
    /// `interrupted(sourceLost)` (S21).
    SourceLost,
    /// The app whose audio is captured quit (S142).
    SourceAppQuit,
    /// `addOutput` threw (S69).
    OutputStartFailed {
        /// The device UID.
        uid: String,
        /// The user caused this start, so an alert is shown too.
        by_user: bool,
        /// Speakers still enabled once this one is disabled.
        enabled_left: usize,
    },
    /// `outputLost(uid)`: the device vanished or its IO stopped for good (S68).
    OutputLost {
        /// The device UID.
        uid: String,
    },
    /// The pipe has no reader or lost it (S89, S92).
    PipeWithoutReader,
    /// The AirPlay engine is down, crashing or unsupported (S98).
    EngineTrouble,
    /// The pairing and reconcile rules dropped an AirPlay speaker (S104 to S109).
    SpeakerDropped {
        /// The engine output ID.
        id: String,
        /// Why.
        reason: DropReason,
        /// Speakers still enabled once this one is disabled.
        enabled_left: usize,
    },
}

/// The host's reaction to an [`Incident`] (S136).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reaction {
    /// Stop the session.
    pub stop_session: bool,
    /// Deselect every AirPlay speaker.
    pub deselect_airplay: bool,
    /// Start a new session right after stopping.
    pub restart: Option<StartOrigin>,
    /// Post (or replace) the notice about this subject.
    pub notice: Option<NoticeSubject>,
    /// The notice is informational rather than a problem.
    pub informational: bool,
    /// Also show a modal alert.
    pub alert: bool,
    /// Remove the speaker from the enabled set.
    pub disable_speaker: bool,
}

/// The reaction to an incident, row by row of the error table (S136).
pub fn reaction(incident: &Incident) -> Reaction {
    let failed_start = |origin: StartOrigin| Reaction {
        stop_session: true,
        deselect_airplay: true,
        notice: Some(NoticeSubject::Start),
        alert: origin == StartOrigin::User,
        ..Reaction::default()
    };
    match incident {
        Incident::NoSpeakerEnabled => Reaction {
            notice: Some(NoticeSubject::Start),
            informational: true,
            ..Reaction::default()
        },
        Incident::MicrophoneDenied { origin } | Incident::StartFailed { origin } => {
            failed_start(*origin)
        }
        Incident::NoAudio => Reaction {
            notice: Some(NoticeSubject::Capture),
            ..Reaction::default()
        },
        Incident::FormatChanged => Reaction {
            stop_session: true,
            restart: Some(StartOrigin::FormatChange),
            ..Reaction::default()
        },
        Incident::SourceLost | Incident::SourceAppQuit => Reaction {
            stop_session: true,
            deselect_airplay: true,
            notice: Some(NoticeSubject::Source),
            ..Reaction::default()
        },
        Incident::OutputStartFailed {
            uid,
            by_user,
            enabled_left,
        } => Reaction {
            stop_session: *enabled_left == 0,
            deselect_airplay: *enabled_left == 0,
            notice: Some(NoticeSubject::Speaker(SpeakerKey::Local(uid.clone()))),
            alert: *by_user,
            disable_speaker: true,
            ..Reaction::default()
        },
        Incident::OutputLost { .. } | Incident::PipeWithoutReader | Incident::EngineTrouble => {
            Reaction::default()
        }
        Incident::SpeakerDropped {
            id,
            reason,
            enabled_left,
        } => Reaction {
            stop_session: *enabled_left == 0,
            deselect_airplay: *enabled_left == 0,
            notice: Some(NoticeSubject::Speaker(SpeakerKey::AirPlay(id.clone()))),
            alert: reason.alerts(),
            disable_speaker: true,
            ..Reaction::default()
        },
    }
}
