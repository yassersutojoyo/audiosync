//! Starting and stopping sessions, keeping their local outputs in sync, source
//! switches, and the host's reaction to everything that can go wrong (S20,
//! S21, S68, S69, S116 to S122, S136, S141, S142, S161 to S163).

use std::collections::BTreeSet;

use super::Seconds;
use super::source::AudioSource;
use super::speakers::{NoticeSubject, SpeakerKey};

/// The most a quit waits in all (S186): the outputs' fade-outs, at most
/// `render::STOP_FADE_CAP`, then whatever the host waits for before it
/// exits, such as the AirPlay sender's teardown.
pub const QUIT_BUDGET: Seconds = 5.0;

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
    /// Add the enabled AirPlay speakers to the AirPlay sender.
    ApplyAirPlaySelection,
    /// Send the master volume to the AirPlay sender (S72).
    SendMasterVolume,
    /// Add every enabled, present local output (S117).
    AddLocalOutputs,
    /// Start the capture check (S18).
    StartCaptureCheck,
}

/// The steps after a successful start, in order (S116).
pub const AFTER_START: [AfterStart; 4] = [
    AfterStart::ApplyAirPlaySelection,
    AfterStart::SendMasterVolume,
    AfterStart::AddLocalOutputs,
    AfterStart::StartCaptureCheck,
];

/// The local outputs to add and remove so that the running renderers match the
/// enabled set (S117, S161).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputSync {
    /// Enabled and present UIDs without a renderer, sorted by UID, each once.
    /// Each is added with its effective delay and gain.
    pub add: Vec<String>,
    /// UIDs with a renderer that are no longer enabled, sorted by UID, each
    /// once.
    pub remove: Vec<String>,
}

impl OutputSync {
    /// Whether nothing needs to change.
    pub fn is_empty(&self) -> bool {
        self.add.is_empty() && self.remove.is_empty()
    }
}

/// Output sync (S117, S161), idempotent, from three sets of device UIDs:
/// `enabled` by the user, `present` (listed now) and `active` (has a
/// renderer). While running it adds `enabled ∩ present − active`; it always
/// removes `active − enabled`. Both lists are sorted by UID, with no limit on
/// their length. An enabled UID that isn't present waits for a later sync,
/// and a renderer whose UID is still enabled is never removed, listed or not.
///
/// Renderers are keyed by device UID, so a device that disappeared (its
/// renderer stopped, its UID still enabled) is added again by the first sync
/// that finds it listed (S68), while the other outputs keep playing, and a
/// change of the system's default output changes nothing (S145).
pub fn sync_outputs<E: AsRef<str>, P: AsRef<str>, A: AsRef<str>>(
    running: bool,
    enabled: &[E],
    present: &[P],
    active: &[A],
) -> OutputSync {
    let enabled: BTreeSet<&str> = enabled.iter().map(AsRef::as_ref).collect();
    let present: BTreeSet<&str> = present.iter().map(AsRef::as_ref).collect();
    let active: BTreeSet<&str> = active.iter().map(AsRef::as_ref).collect();
    let add = if running {
        enabled
            .iter()
            .filter(|uid| present.contains(*uid) && !active.contains(*uid))
            .map(|uid| (*uid).to_owned())
            .collect()
    } else {
        Vec::new()
    };
    let remove = active
        .difference(&enabled)
        .map(|uid| (*uid).to_owned())
        .collect();
    OutputSync { add, remove }
}

/// The kind of speaker the user switched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeakerKind {
    /// A local output device.
    Local,
    /// An AirPlay speaker of the AirPlay sender.
    AirPlay,
}

/// What turning a speaker on does (S119).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnOn {
    /// Not running: start a session with user origin.
    StartSession,
    /// Running, local speaker: sync the outputs in (S117).
    SyncLocalOutputs,
    /// Running, AirPlay speaker: add it to the AirPlay sender.
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
    /// Remove it from the AirPlay sender.
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

/// Something that may start or stop streaming (S162).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeakerEvent {
    /// The user switched a speaker on.
    SwitchedOn,
    /// The user switched a speaker off.
    SwitchedOff,
    /// The user asked to start streaming.
    StartRequested,
    /// A speaker's device or AirPlay entry went away or came back (unplugged,
    /// out of range, listed again); nobody switched a speaker.
    ListingChanged,
}

/// What streaming does after a [`SpeakerEvent`] (S162).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamingAction {
    /// Start a session, user origin (S119).
    Start,
    /// Stop the session and deselect every AirPlay speaker (S118).
    Stop,
    /// Start nothing; ask the user to choose a speaker first with an
    /// informational "start" notice (S116, S136).
    AskForSpeaker,
    /// Nothing changes.
    Nothing,
}

/// The streaming policy (S162), decided from the enabled sets alone:
/// `enabled_local` and `enabled_airplay` count the speakers enabled after
/// the event, whether their devices are listed or not.
///
/// A session runs while anything at all is enabled, so a speaker that is on
/// but unplugged or out of reach keeps it running, and one AirPlay speaker is
/// enough; it stops once nothing is enabled. Only switching a speaker on, or
/// asking to start, while nothing streams starts a session; a start with
/// nothing enabled asks for a speaker instead.
pub fn streaming_after(
    event: SpeakerEvent,
    streaming: bool,
    enabled_local: usize,
    enabled_airplay: usize,
) -> StreamingAction {
    let any_enabled = enabled_local > 0 || enabled_airplay > 0;
    match (event, streaming, any_enabled) {
        (_, true, false) => StreamingAction::Stop,
        (SpeakerEvent::StartRequested, false, false) => StreamingAction::AskForSpeaker,
        (SpeakerEvent::SwitchedOn | SpeakerEvent::StartRequested, false, true) => {
            StreamingAction::Start
        }
        _ => StreamingAction::Nothing,
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
    /// race the reselect).
    pub stop_keeping_airplay: bool,
    /// Then start a session with this origin (ring reset, renderers added
    /// again).
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

/// Notices about a source that is being replaced (S163).
static OLD_SOURCE_NOTICES: [NoticeSubject; 2] = [NoticeSubject::Source, NoticeSubject::Capture];

/// The host's answer when the user picks a capture source (S163), from the
/// least to the most work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourcePick {
    /// The pick is the source already in use, so there is nothing to do.
    Nothing,
    /// No session runs: the pick becomes the stored source and waits for the
    /// next start to capture it.
    StoreForNextStart,
    /// A session runs: the pick becomes the stored source and the capture
    /// moves to it right away, following this switch (S141). Whatever kind
    /// the new source is, input devices included, the move keeps every
    /// speaker on and every AirPlay speaker selected.
    SwitchNow(SourceSwitch),
}

impl SourcePick {
    /// The notices this pick makes obsolete: when the source changes, the old
    /// source's "source" and "capture" notices no longer apply; when it
    /// doesn't, nothing is cleared.
    pub fn clears(&self) -> &'static [NoticeSubject] {
        match self {
            SourcePick::Nothing => &[],
            SourcePick::StoreForNextStart | SourcePick::SwitchNow(_) => &OLD_SOURCE_NOTICES,
        }
    }
}

/// What picking `picked` does while `current` is the stored source;
/// `streaming` says whether a session runs (S163). A pick that differs from
/// `current` in kind or identifier always changes the source; whether the
/// capture follows now or at the next start depends only on `streaming`.
pub fn source_pick(current: &AudioSource, picked: &AudioSource, streaming: bool) -> SourcePick {
    if picked == current {
        SourcePick::Nothing
    } else if !streaming {
        SourcePick::StoreForNextStart
    } else {
        SourcePick::SwitchNow(source_switch(true))
    }
}

/// Why an AirPlay speaker the user enabled can't play, so the host turns it
/// off (S136).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DropReason {
    /// The AirPlay sender can't reach it.
    Unreachable,
    /// It needs a PIN or password that hasn't been entered.
    NeedsCode,
    /// It still needs a code after pairing: the code was rejected.
    CodeRejected,
}

impl DropReason {
    /// Only a rejected code also gets an alert; every drop gets a notice (S136).
    pub fn alerts(self) -> bool {
        self == DropReason::CodeRejected
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
    /// The AirPlay sender can't play an AirPlay speaker the user enabled.
    SpeakerDropped {
        /// The speaker's receiver ID.
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
        Incident::OutputLost { .. } => Reaction::default(),
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
