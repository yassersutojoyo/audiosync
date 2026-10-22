//! What the interface shows and what it can ask for, independent of how the
//! audio work is done.
//!
//! The UI reads an [`AppState`] snapshot from a [`Backend`] and calls the
//! backend's methods for every user action, then reads a new snapshot. On
//! Windows the backend is `crate::adapter::AudioAdapter` over the WASAPI audio
//! backend; elsewhere [`MockBackend`] stands in with example data, for work on
//! the UI. Nothing here makes a sound or opens an audio device.

use audiosync_core::eq::{self, EqSettings};
use audiosync_core::policy::device_volume::Hint;
use audiosync_core::policy::source_quit::WhenSourceQuits;
#[cfg(any(not(windows), test))]
use audiosync_core::volume;

use audiosync_core::calibrate_all::{AirPlayShared, Class, EndReason, RowState};

pub use crate::audio::backend::CastMembers;
use crate::audio::calibration::{Failure, Phase};

pub use crate::settings::Appearance;

/// The largest offset the sync controls accept, in milliseconds (the macOS
/// app's limit too).
pub const MAX_DELAY_MS: f64 = 20_000.0;

/// The smallest offset the sync controls accept, in milliseconds: a speaker
/// that sounds up to two seconds late.
pub const MIN_DELAY_MS: f64 = -2000.0;

/// The most notices kept at once; the oldest drops off past it.
#[cfg(test)]
pub const NOTICE_CAPACITY: usize = 4;

/// Where a source's audio comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// Everything the PC plays.
    System,
    /// One app's audio.
    App,
    /// An input device (line in, microphone).
    Input,
}

/// Something AudioSync can capture.
#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    pub id: String,
    pub name: String,
    pub kind: SourceKind,
}

/// The kind of output a speaker is, shown as a small icon and read by Narrator.
// The mock lists only a few kinds.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeakerKind {
    AirPlay,
    /// An AirPlay speaker AudioSync reaches over AirPlay 2.
    AirPlay2,
    /// A Google Cast speaker.
    Cast,
    BuiltIn,
    Headphones,
    Bluetooth,
    Usb,
    /// Shown with the headphones glyph and "USB" as its kind.
    UsbHeadphones,
    /// Shown with the speakers glyph and "USB" as its kind.
    UsbSpeakers,
    Hdmi,
    DisplayPort,
    Virtual,
    Other,
}

impl SpeakerKind {
    /// The PC's own outputs, listed under "This PC" (like "This Mac").
    pub fn is_this_pc(self) -> bool {
        matches!(self, SpeakerKind::BuiltIn | SpeakerKind::Headphones)
    }
}

/// Whether an AirPlay speaker can simply play.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    Open,
    /// It asks for a PIN or a password, which the user types when switching
    /// it on.
    NeedsCode,
    /// A pairing with it runs: the PIN prompt is open or about to open.
    #[cfg_attr(
        all(not(windows), not(test)),
        expect(dead_code, reason = "only the Windows backend pairs")
    )]
    Pairing,
    /// It was switched on while another Apple TV has the PIN prompt; its
    /// pairing gets the prompt once that one ends.
    #[cfg_attr(
        all(not(windows), not(test)),
        expect(dead_code, reason = "only the Windows backend pairs")
    )]
    WaitingToPair,
    /// It needs a kind of pairing AudioSync for Windows can't do yet.
    #[cfg_attr(
        all(not(windows), not(test)),
        expect(dead_code, reason = "only the Windows backend lists real receivers")
    )]
    PairingNotYet,
    /// Only AirPlay 2 can play it, which AudioSync for Windows doesn't speak
    /// yet.
    #[cfg_attr(
        all(not(windows), not(test)),
        expect(dead_code, reason = "only the Windows backend lists real receivers")
    )]
    AirPlay2NotYet,
    /// It wants an encryption, codec or format AudioSync can't send.
    #[cfg_attr(
        all(not(windows), not(test)),
        expect(dead_code, reason = "only the Windows backend lists real receivers")
    )]
    Unsupported,
}

/// What went wrong with an AirPlay speaker, as a notice words it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    all(not(windows), not(test)),
    expect(dead_code, reason = "only the Windows backend posts notices")
)]
pub enum AirPlayProblem {
    /// It doesn't answer; AudioSync keeps trying.
    NotResponding,
    /// The connection was lost while it played; AudioSync keeps trying.
    ConnectionLost,
    /// Another device seems to play to it; AudioSync keeps trying.
    Busy,
    /// It turned the stream down; AudioSync tries again.
    Refused,
    /// It asks for a password AudioSync doesn't have, or turned the one it
    /// had down; switching it on asks for it.
    NeedsPassword,
    /// An Apple TV that needs pairing with a PIN first; switching it on
    /// pairs it.
    NeedsPin,
    /// Pairing with an Apple TV failed, so it is off.
    PairingFailed,
    /// It needs pairing, which AudioSync for Windows can't do yet.
    PairingNotYet,
    /// Only AirPlay 2 can play it.
    AirPlay2NotYet,
    /// It turned NTP timing down, and PTP timing isn't available.
    TimingUnavailable,
    /// Another AirPlay 2 speaker plays; it starts once that one stops.
    OneAirPlay2Speaker,
    /// An Apple TV whose home theater group plays, so it is off.
    PlaysThroughSpeakers,
    /// A home theater group whose Apple TV plays by itself, so it is off.
    AppleTvPlays,
    /// A member of a pair or group that plays as part of it, so it is off.
    PlaysInGroup,
    /// AudioSync can't play to it.
    Unsupported,
}

/// One output: a local device or an AirPlay speaker.
#[derive(Clone, Debug, PartialEq)]
pub struct Speaker {
    pub id: String,
    pub name: String,
    pub kind: SpeakerKind,
    /// Chosen by the user; it plays while streaming is on.
    pub enabled: bool,
    /// 0 to 100.
    pub volume: f64,
    /// The offset of a local output relative to the reference; the
    /// reference has none, and neither do AirPlay speakers (the AirPlay
    /// sender keeps them in sync, and local outputs follow it).
    pub delay_ms: Option<f64>,
    pub auto_connect: bool,
    pub availability: Availability,
    /// An Apple TV AudioSync keeps pairing credentials for, which the user
    /// can forget.
    pub paired: bool,
    /// A local output that is on, whose own volume in Windows is low or
    /// muted (spec S196): the hint under its volume slider, with the fix.
    pub device_volume: Option<Hint>,
    /// Why a speaker that is on can't play right now, while streaming is
    /// on; it plays again by itself. `None` while it plays or is off.
    pub waiting: Option<Waiting>,
    /// What only a Google Cast speaker's row shows.
    pub cast: Option<CastRow>,
    /// A stereo pair or home theater group: what kind, and its members.
    pub group: Option<crate::audio::backend::AirPlayGroup>,
}

/// A Google Cast speaker's own state. Its delay is the measured offset the
/// user sets, since Cast can't be kept in sync automatically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CastRow {
    pub state: CastState,
    /// The speaker reports itself muted.
    pub muted: bool,
    /// The speaker's volume can't be changed from here.
    pub volume_fixed: bool,
    /// Set for a Cast group, which plays as one speaker.
    pub group: Option<CastGroup>,
}

/// A Google Cast group and what its row knows of its members.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CastGroup {
    pub members: CastMembers,
}

/// Where a Google Cast speaker that is on is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    all(not(windows), not(test)),
    expect(dead_code, reason = "only the Windows backend plays to Cast")
)]
pub enum CastState {
    /// Off, or on while streaming is off.
    Idle,
    Connecting,
    Playing,
    /// It played, and AudioSync is reaching it again.
    Reconnecting,
}

/// What the speaker list says about Google Cast while no Cast speaker is
/// listed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    all(not(windows), not(test)),
    expect(dead_code, reason = "only the Windows backend looks for Cast")
)]
pub enum CastList {
    /// Nothing: a Cast speaker is listed, or Cast isn't running.
    #[default]
    Quiet,
    /// Discovery runs and has found nothing yet.
    Searching,
    /// Discovery has found nothing for a while.
    NoneFound,
    /// Discovery couldn't start or stopped.
    Failed,
}

/// Why a Google Cast speaker can't play, as a notice words it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    all(not(windows), not(test)),
    expect(dead_code, reason = "only the Windows backend posts notices")
)]
pub enum CastProblem {
    /// Another Cast speaker is on; one plays at a time.
    Busy,
    NotResponding,
    /// It turned the stream down.
    Refused,
    /// Another app started casting to it.
    TakenOver,
    ConnectionLost,
}

/// Why a speaker that is on can't play right now. Its row says so instead
/// of showing it as playing; it stays on and plays again by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    all(not(windows), not(test)),
    expect(
        dead_code,
        reason = "only the Windows backend reports a waiting speaker"
    )
)]
pub enum Waiting {
    /// Another app holds its device for itself (exclusive mode); it plays
    /// once that app lets go.
    ForDevice,
    /// An AirPlay speaker that stopped playing, which AudioSync keeps trying
    /// to reach.
    Reconnecting,
    /// An AirPlay speaker that waits for the one AirPlay 2 slot; it starts
    /// once the other AirPlay 2 speaker stops.
    ForAirPlay2Slot,
}

/// Where a pairing's PIN prompt is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    all(not(windows), not(test)),
    expect(dead_code, reason = "only the Windows backend pairs")
)]
pub enum PinPhase {
    /// The Apple TV is asked to show a PIN.
    Starting,
    /// The Apple TV shows a PIN for the user to type.
    Ready,
    /// The PIN went to the Apple TV, which checks it.
    Checking,
}

/// The PIN prompt of a running pairing: a dialog asks for the PIN an Apple
/// TV shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinPrompt {
    /// The speaker's ID.
    pub speaker_id: String,
    /// The speaker's name.
    pub name: String,
    pub phase: PinPhase,
    /// The Apple TV turned the last PIN down and shows a new one.
    pub wrong_pin: bool,
}

/// The password prompt of an AirPlay speaker the user switched on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasswordPrompt {
    /// The speaker's ID.
    pub speaker_id: String,
    /// The speaker's name.
    pub name: String,
    /// The speaker turned the last password down.
    pub wrong: bool,
}

impl Speaker {
    /// Whether the speaker lists show it. A virtual output (a loopback
    /// driver, a voice changer, a meeting app's device) is listed only while
    /// Show Virtual Devices is on or while it is turned on. The main window
    /// also keeps one listed once it was shown (`ListedVirtual` in
    /// src/view.rs), so turning it off doesn't remove its row.
    pub fn is_listed(&self, show_virtual: bool) -> bool {
        show_virtual || self.enabled || self.kind != SpeakerKind::Virtual
    }
}

/// A named set of speakers switched together.
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub members: Vec<String>,
}

/// A problem worth telling the user about. A newer notice about the same
/// subject replaces the older one.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub id: u64,
    pub subject: String,
    pub message: NoticeMessage,
    /// Local time it was posted, in the user's short time format ("09.40",
    /// "9:40 AM").
    pub time: String,
}

/// What a notice says. The UI words it (the notice texts in
/// `ui/strings.slint`), so no backend builds user-facing text.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(
    all(not(windows), not(test)),
    expect(dead_code, reason = "only the Windows backend posts notices")
)]
pub enum NoticeMessage {
    /// Streaming was asked to start with no speaker on.
    NoSpeaker,
    /// The app being captured quit, so streaming stopped.
    SourceQuit { source: String },
    /// The app being captured quit, so streaming went on with the whole
    /// system (spec S227).
    SourceSwitched { source: String },
    /// The chosen app isn't running, so streaming didn't start.
    SourceNotRunning { source: String },
    /// Capturing the chosen app would capture AudioSync's own sound too, so
    /// streaming didn't start.
    SourceRefused { source: String },
    /// The chosen input device isn't connected, so streaming didn't start.
    InputNotConnected { source: String },
    /// The input device being captured was disconnected, so streaming
    /// stopped.
    InputDisconnected { source: String },
    /// Windows' privacy settings block microphone access for desktop apps,
    /// so the input device can't be captured. The notice opens those
    /// settings.
    MicrophoneBlocked { source: String },
    /// Capture couldn't start.
    CaptureFailed,
    /// Capture stopped working while streaming, so streaming stopped.
    CaptureLost,
    /// A speaker couldn't start, so it was turned off.
    SpeakerFailed { speaker: String },
    /// A speaker's device went away while it played. It stays on and plays
    /// again once the device is back.
    SpeakerLost { speaker: String },
    /// Another app holds a speaker's device for itself (exclusive mode). It
    /// stays on and plays once that app lets go; the notice then clears.
    SpeakerBusy { speaker: String },
    /// An AirPlay speaker stopped playing; it stays on while AudioSync keeps
    /// trying to reach it.
    AirPlayRetrying {
        speaker: String,
        problem: AirPlayProblem,
    },
    /// An AirPlay speaker can't play from AudioSync, so it is off.
    AirPlayCantPlay {
        speaker: String,
        problem: AirPlayProblem,
    },
    /// AudioSync's AirPlay sender stopped after an internal error or
    /// couldn't start, so AirPlay speakers aren't listed or playing. The
    /// notice offers Retry ([`Backend::retry_airplay`]).
    AirPlayStopped,
    /// The settings file couldn't be written, so changes made now last only
    /// until AudioSync quits. Posted once until a save works again, which
    /// clears it.
    SettingsNotSaved,
    /// The pairings' file couldn't be written, so pairings added or
    /// forgotten now last only until AudioSync quits. Posted once until a
    /// write works again, which clears it.
    PairingsNotSaved,
    /// The settings file couldn't be read at launch (or was damaged), so
    /// AudioSync started with the defaults; once it could be read again, it
    /// was kept aside as `file`, in the app's data folder, before the first
    /// save over it.
    SettingsKeptAside { file: String },
    /// The pairings' file couldn't be read at launch (or decrypted, or
    /// parsed), so AudioSync started without the pairings in it; once it
    /// could be read again, it was kept aside as `file`, in the app's data
    /// folder, before the first write over it.
    PairingsKeptAside { file: String },
    /// Windows has no active sound output while an AirPlay speaker is on, so
    /// apps have nowhere to play and nothing reaches the speaker. Clears
    /// itself once an output is back or no AirPlay speaker is on.
    NoSoundOutput,
    /// The source also plays straight from these outputs, undelayed, so it
    /// is heard twice. `source` is the app's name, or `None` for the whole
    /// system. `mute` names the outputs AudioSync doesn't play to, which the
    /// user may mute; `shared` the ones AudioSync plays to as well, where
    /// only sending the source elsewhere helps. `default_output` is the one
    /// of them that counts because it is Windows' default output (the whole
    /// system only); any other plays the source because an app plays there.
    HeardTwice {
        source: Option<String>,
        mute: Vec<String>,
        shared: Vec<String>,
        default_output: Option<String>,
    },
    /// Windows refused the Run value, so Launch at Sign-In stays off.
    LaunchAtLoginFailed,
    /// Windows refused to remove the Run value, so Launch at Sign-In stays on.
    LaunchAtLoginNotRemoved,
    /// The Run value is written, but Task Manager or Settings has AudioSync
    /// turned off as a startup app; it is turned on again there.
    LaunchAtLoginOffInSettings,
    /// A Google Cast speaker can't play, so it is off.
    CastCantPlay {
        speaker: String,
        problem: CastProblem,
    },
    /// The AirPlay speakers can't wait long enough for the Google Cast
    /// speaker that is on, so they play about `tenths` tenths of a second
    /// ahead of it. Clears itself once they keep up again.
    AirPlayAheadOfCast { speaker: String, tenths: u32 },
}

/// How a speaker or group row shows: playing only while streaming is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeakerState {
    Off,
    /// Chosen, and plays once streaming starts.
    WillPlay,
    Playing,
    /// On while streaming is on, but it can't play right now (a group:
    /// none of its present members can).
    #[cfg_attr(
        all(not(windows), not(test)),
        expect(
            dead_code,
            reason = "only the Windows backend reports a waiting speaker"
        )
    )]
    Waiting(Waiting),
}

impl SpeakerState {
    pub fn new(enabled: bool, streaming: bool) -> Self {
        match (enabled, streaming) {
            (false, _) => SpeakerState::Off,
            (true, false) => SpeakerState::WillPlay,
            (true, true) => SpeakerState::Playing,
        }
    }

    /// What a click on the row does: off or waiting for streaming turns on;
    /// playing, or on but waiting for its device, turns off.
    pub fn click_turns_on(self) -> bool {
        matches!(self, SpeakerState::Off | SpeakerState::WillPlay)
    }
}

/// Everything the windows show.
#[derive(Clone, Debug, PartialEq)]
pub struct AppState {
    /// Streaming is on.
    pub running: bool,
    pub sources: Vec<Source>,
    pub source_id: String,
    /// 0 to 100.
    pub master_volume: f64,
    pub speakers: Vec<Speaker>,
    pub groups: Vec<Group>,
    pub equalizer: EqSettings,
    /// Newest first.
    pub notices: Vec<Notice>,
    /// The PIN prompt to show, while a pairing asks for a PIN.
    pub pin_prompt: Option<PinPrompt>,
    /// The password prompt to show, while a speaker asks for its password.
    pub password_prompt: Option<PasswordPrompt>,
    /// The name of the reference output, which the other outputs' offsets
    /// are relative to; `None` without one (spec S174, S181).
    pub reference_name: Option<String>,
    /// The AirPlay delay field's value L in milliseconds, while an AirPlay
    /// sender runs (spec S178, S181).
    pub airplay_delay_ms: Option<f64>,
    /// AirPlay discovery runs but has found no speaker for a while: the
    /// speaker list says to check the network and Windows Firewall.
    pub airplay_none_found: bool,
    /// What a session does when the app it captures quits (spec S227).
    pub when_source_quits: WhenSourceQuits,
    /// The speaker list's note about Google Cast discovery.
    pub cast_list: CastList,
    /// The microphone calibration a row shows.
    pub calibration: CalibrationState,
    /// Calibrate All Speakers, in its own card.
    pub calibrate_all: CalibrateAllState,
}

/// Calibrate All Speakers (spec S235 to S240): where a run stands and the
/// results of the last one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CalibrateAllState {
    pub phase: AllPhase,
    /// Why a run can't start now; `None` when the button may try.
    pub blocked: Option<Failure>,
    /// The results, while they still describe the stored delays.
    pub sheet: Option<SheetState>,
    /// Counts each ending, Apply and Undo, so Narrator hears each once.
    pub endings: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum AllPhase {
    #[default]
    Idle,
    Running(AllProgress),
    /// It couldn't start, or the microphone or the capture stopped it.
    Failed(Failure),
    /// A delay changed after the run, so its results went.
    Stale,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AllProgress {
    /// The music fades out and plays out.
    Preparing,
    /// The sweep plays on speaker `name` alone: the reference's one sweep,
    /// or sweep `take` (1 or 2) of another speaker.
    Measuring {
        name: String,
        reference: bool,
        take: usize,
        searching: bool,
    },
    Estimating,
}

/// The results of a run.
#[derive(Clone, Debug, PartialEq)]
pub struct SheetState {
    pub rows: Vec<SheetRow>,
    pub airplay: Option<AirPlayShared>,
    /// Why the run ended before it could be applied.
    pub ended: Option<EndReason>,
    /// The microphone or the capture stopped the run.
    pub failure: Option<Failure>,
    /// The last Measure Again ended early, so its row stayed.
    pub again_ended: Option<EndReason>,
    pub can_apply: bool,
    /// Apply or Undo would set the AirPlay delay while AirPlay doesn't
    /// play, so it can't be worked out: both wait.
    pub airplay_waits: bool,
    pub applied: bool,
    /// Undo restores what Apply replaced, in one step.
    pub can_undo: bool,
    pub undone: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SheetRow {
    pub name: String,
    pub class: Class,
    /// The stored delay before the run.
    pub old_ms: Option<f64>,
    pub state: RowState,
    pub can_measure_again: bool,
}

/// The microphone calibration of one local speaker (spec S232): where it
/// stands, on the row of the speaker it concerns.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CalibrationState {
    /// The speaker it concerns; `None` when none ran.
    pub speaker_id: Option<String>,
    pub phase: Phase,
    /// A run holds the outputs (it runs, or a cut-off sweep plays out),
    /// this one or Calibrate All: no other may start.
    pub busy: bool,
}

impl AppState {
    pub fn speaker(&self, id: &str) -> Option<&Speaker> {
        self.speakers.iter().find(|s| s.id == id)
    }

    /// A speaker that is on while streaming is on plays, unless it waits.
    pub fn speaker_state(&self, speaker: &Speaker) -> SpeakerState {
        match SpeakerState::new(speaker.enabled, self.running) {
            SpeakerState::Playing => speaker
                .waiting
                .map_or(SpeakerState::Playing, SpeakerState::Waiting),
            state => state,
        }
    }

    /// The members of `group` that are present right now.
    pub fn available_members<'a>(&'a self, group: &'a Group) -> impl Iterator<Item = &'a Speaker> {
        group.members.iter().filter_map(|id| self.speaker(id))
    }

    /// A group is on when all its present members are. Playing, it waits
    /// when every present member waits, as its first present member does.
    pub fn group_state(&self, group: &Group) -> SpeakerState {
        let mut present = self.available_members(group).peekable();
        let all_on = present.peek().is_some() && present.all(|s| s.enabled);
        match SpeakerState::new(all_on, self.running) {
            SpeakerState::Playing => {
                let waiting: Option<Vec<Waiting>> =
                    self.available_members(group).map(|s| s.waiting).collect();
                waiting
                    .and_then(|w| w.first().copied())
                    .map_or(SpeakerState::Playing, SpeakerState::Waiting)
            }
            state => state,
        }
    }

    /// Speakers switched on right now.
    pub fn active_speakers(&self) -> usize {
        self.speakers.iter().filter(|s| s.enabled).count()
    }

    /// The speaker lists both windows show: the PC's own outputs in their
    /// order, then every other speaker by name, case-insensitively.
    pub fn layout(&self) -> (Vec<&Speaker>, Vec<&Speaker>) {
        let this_pc = self
            .speakers
            .iter()
            .filter(|s| s.kind.is_this_pc())
            .collect();
        let mut connected: Vec<&Speaker> = self
            .speakers
            .iter()
            .filter(|s| !s.kind.is_this_pc())
            .collect();
        // A stable sort keeps the order of equal names.
        connected.sort_by_key(|s| s.name.to_lowercase());
        (this_pc, connected)
    }
}

/// Why a new group wasn't made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupError {
    /// No speaker is on, so the group would be empty.
    NoActiveSpeakers,
}

/// The audio side, as the UI sees it. Every method is called on the UI
/// thread, and the next snapshot shows what it did. Most calls return at
/// once, but on Windows a call that starts or stops streams waits for them:
/// starting or stopping streaming, picking another source while streaming,
/// turning a speaker or group on or off while streaming, and a poll that
/// brings a speaker back. Each stream takes a moment to open (longer for a
/// Bluetooth device, and starting capture waits up to 5 s for Windows), and
/// the windows don't respond meanwhile.
pub trait Backend {
    fn snapshot(&self) -> AppState;
    fn set_running(&mut self, on: bool);
    fn select_source(&mut self, id: &str);
    fn set_master_volume(&mut self, volume: f64);
    fn set_speaker_enabled(&mut self, id: &str, on: bool);
    fn set_speaker_volume(&mut self, id: &str, volume: f64);
    fn set_speaker_delay(&mut self, id: &str, ms: f64);
    /// Mutes or unmutes a Google Cast speaker on the speaker itself.
    fn set_speaker_muted(&mut self, _id: &str, _muted: bool) {}
    /// The Resync button of a Google Cast speaker: it restarts its stream so
    /// it plays at its set offset again.
    fn resync_speaker(&mut self, _id: &str) {}
    /// The user entered `ms` in the AirPlay delay field.
    fn set_airplay_delay(&mut self, ms: f64);
    fn set_auto_connect(&mut self, id: &str, on: bool);
    /// Turns every present member on, or all of them off when the group is on.
    fn toggle_group(&mut self, id: &str);
    /// Makes a group of the speakers that are on now. The UI names it
    /// ("Group 2") when the user leaves the name empty.
    fn new_group(&mut self, name: &str) -> Result<(), GroupError>;
    fn rename_group(&mut self, id: &str, name: &str);
    fn delete_group(&mut self, id: &str);
    fn set_equalizer(&mut self, settings: EqSettings);
    fn dismiss_notice(&mut self, id: u64);
    fn dismiss_all_notices(&mut self);
    fn refresh_devices(&mut self);
    fn set_when_source_quits(&mut self, setting: WhenSourceQuits);

    /// The fix under speaker `id`'s device-volume hint: raises its own
    /// volume in Windows to 75 % when low and unmutes it when muted
    /// (spec S197).
    fn fix_device_volume(&mut self, _id: &str) {}

    /// The PIN the user typed for the pairing with speaker `id`.
    fn submit_pin(&mut self, _id: &str, _pin: &str) {}

    /// The user cancelled the pairing with speaker `id`; it stays off.
    fn cancel_pairing(&mut self, _id: &str) {}

    /// The password the user typed for speaker `id`.
    fn submit_password(&mut self, _id: &str, _password: &str) {}

    /// The user cancelled the password prompt of speaker `id`; it is off.
    fn cancel_password(&mut self, _id: &str) {}

    /// Forgets the pairing with speaker `id`: it asks for a PIN again next
    /// time.
    fn forget_pairing(&mut self, _id: &str) {}

    /// Retry on the notice that AirPlay stopped: starts a new AirPlay
    /// sender, and the speakers that were on come back once it lists them.
    fn retry_airplay(&mut self) {}

    /// Calibrate with Microphone on local speaker `id`'s row.
    fn start_calibration(&mut self, _id: &str) {}

    /// Cancel on the running calibration: its speaker's delay stays.
    fn cancel_calibration(&mut self) {}

    /// Calibrate All Speakers: measures every speaker that is on.
    fn start_calibrate_all(&mut self) {}

    /// Cancel on the running Calibrate All or Measure Again: nothing
    /// changes.
    fn cancel_calibrate_all(&mut self) {}

    /// Measure Again on the results' row `row`.
    fn measure_again(&mut self, _row: usize) {}

    /// Apply on the results: the certain rows' delays, as one change.
    fn apply_calibrate_all(&mut self) {}

    /// Undo after Apply: every replaced delay back, in one step.
    fn undo_calibrate_all(&mut self) {}

    /// Close on the results.
    fn close_calibrate_all(&mut self) {}

    /// Moves a calibration on; the UI calls it every few tens of ms while
    /// [`CalibrationState::busy`]. Returns whether the snapshot changed.
    fn tick_calibration(&mut self) -> bool {
        false
    }

    /// Takes in what happened since the last call (devices that came or went,
    /// a source that quit, an output that failed) and says whether the
    /// snapshot may have changed. The UI calls it about twice a second.
    fn poll(&mut self) -> bool {
        false
    }

    /// The windows' own settings as last saved: what the UI shows them with
    /// at launch.
    fn view_settings(&self) -> ViewSettings {
        ViewSettings::default()
    }

    /// The windows' own settings changed; they are saved with the rest.
    fn set_view_settings(&mut self, _settings: ViewSettings) {}

    /// Saves the settings now, without waiting for the next poll: the app
    /// is about to quit.
    fn save_settings(&mut self) {}
}

/// Where the main window was and how big: the position of its outer frame
/// in physical pixels, its inner size in logical pixels, both as it last
/// showed normally; and whether it was maximized (a file from before this
/// flag reads as not maximized).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WindowFrame {
    pub x: i32,
    pub y: i32,
    pub width: f64,
    pub height: f64,
    #[serde(default)]
    pub maximized: bool,
}

/// How the main window shows when its frame is remembered.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MainWindowShows {
    /// Hidden or minimized: nothing to learn from it.
    Not,
    /// Shown normally, with this frame.
    Normal(WindowFrame),
    /// Maximized; the frame it covers now.
    Maximized(WindowFrame),
}

/// The main window frame to keep, given how the window `shows` and the frame
/// kept so far (`saved`): a normal window's own frame; for a maximized one,
/// the last normal frame marked maximized (or, with none yet, the maximized
/// frame itself); for a hidden or minimized one, what was kept.
pub fn frame_to_keep(shows: MainWindowShows, saved: Option<WindowFrame>) -> Option<WindowFrame> {
    match shows {
        MainWindowShows::Not => saved,
        MainWindowShows::Normal(frame) => Some(WindowFrame {
            maximized: false,
            ..frame
        }),
        MainWindowShows::Maximized(frame) => Some(WindowFrame {
            maximized: true,
            ..saved.unwrap_or(frame)
        }),
    }
}

/// What the windows themselves keep across restarts.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ViewSettings {
    /// View > Show Virtual Devices.
    pub show_virtual_devices: bool,
    /// View > Appearance.
    pub appearance: Appearance,
    /// The main window's place and size, once it has been shown.
    pub window: Option<WindowFrame>,
    /// File > Web Remote.
    pub remote: crate::remote::RemoteSettings,
}

/// Clamps a typed or stepped offset to what the sync controls accept, in
/// whole milliseconds.
pub fn clamp_delay_ms(ms: f64) -> f64 {
    if ms.is_nan() {
        0.0
    } else {
        ms.clamp(MIN_DELAY_MS, MAX_DELAY_MS).round()
    }
}

/// Parses what the user typed in a milliseconds field ("120", "+120",
/// "−250", "-250", "120,5"), rounded to whole milliseconds; `None` while it
/// isn't a number yet.
pub fn parse_ms(text: &str) -> Option<f64> {
    let value: f64 = text
        .trim()
        .replace(',', ".")
        .replace('\u{2212}', "-")
        .parse()
        .ok()?;
    value.is_finite().then(|| value.round())
}

/// Parses what the user typed in an offset field, clamped to what the sync
/// controls accept; `None` while it isn't a number yet.
pub fn parse_delay_ms(text: &str) -> Option<f64> {
    parse_ms(text).map(clamp_delay_ms)
}

/// What a delay field keeps of a keystroke or a paste: digits, after one
/// leading "+" or "−" (or hyphen-minus); anything else can't be typed.
pub fn delay_field_text(typed: &str) -> String {
    typed
        .chars()
        .enumerate()
        .filter(|&(index, c)| {
            c.is_ascii_digit() || (index == 0 && matches!(c, '+' | '-' | '\u{2212}'))
        })
        .map(|(_, c)| c)
        .collect()
}

/// Example data for UI work and tests: no device is opened, nothing plays.
#[cfg(any(not(windows), test))]
pub struct MockBackend {
    state: AppState,
    next_id: u64,
    view: ViewSettings,
}

#[cfg(any(not(windows), test))]
impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(not(windows), test))]
impl MockBackend {
    pub fn new() -> Self {
        let source = |id: &str, name: &str, kind| Source {
            id: id.into(),
            name: name.into(),
            kind,
        };
        let speaker = |id: &str, name: &str, kind, enabled, delay_ms| Speaker {
            id: id.into(),
            name: name.into(),
            kind,
            enabled,
            volume: volume::DEFAULT_LOCAL_POSITION,
            delay_ms,
            auto_connect: false,
            availability: Availability::Open,
            paired: false,
            device_volume: None,
            waiting: None,
            cast: None,
            group: None,
        };
        let mut speakers = vec![
            // The reference: no delay of its own.
            speaker(
                "local:speakers",
                "Speakers",
                SpeakerKind::BuiltIn,
                true,
                None,
            ),
            speaker(
                "local:headphones",
                "Headphones",
                SpeakerKind::Headphones,
                false,
                Some(0.0),
            ),
            speaker(
                "airplay:kitchen",
                "Kitchen",
                SpeakerKind::AirPlay,
                true,
                None,
            ),
            speaker(
                "airplay:living",
                "Living Room",
                SpeakerKind::AirPlay,
                false,
                None,
            ),
            speaker(
                "local:desk",
                "Desk Speaker",
                SpeakerKind::Usb,
                false,
                Some(120.0),
            ),
            speaker(
                "local:bt",
                "Headset",
                SpeakerKind::Bluetooth,
                false,
                Some(-250.0),
            ),
            speaker(
                "airplay:bedroom",
                "Bedroom",
                SpeakerKind::AirPlay,
                false,
                None,
            ),
        ];
        speakers[3].availability = Availability::NeedsCode;
        speakers[2].auto_connect = true;
        // The PC's speakers left at 13 % in Windows.
        speakers[0].device_volume = Some(Hint::Low { percent: 13 });
        let state = AppState {
            running: false,
            sources: vec![
                source("system", "All System Audio", SourceKind::System),
                source("app:player", "Music Player", SourceKind::App),
                source("app:browser", "Web Browser", SourceKind::App),
                source("input:line", "Line In", SourceKind::Input),
            ],
            source_id: "system".into(),
            master_volume: volume::DEFAULT_MASTER_POSITION,
            speakers,
            groups: vec![Group {
                id: "group:1".into(),
                name: "Downstairs".into(),
                members: vec!["airplay:kitchen".into(), "airplay:living".into()],
            }],
            equalizer: EqSettings::default(),
            notices: Vec::new(),
            pin_prompt: None,
            password_prompt: None,
            reference_name: Some("Speakers".into()),
            airplay_delay_ms: Some(1750.0),
            airplay_none_found: false,
            when_source_quits: WhenSourceQuits::Stop,
            cast_list: CastList::Quiet,
            calibration: CalibrationState::default(),
            calibrate_all: CalibrateAllState::default(),
        };
        MockBackend {
            state,
            next_id: 1,
            view: ViewSettings::default(),
        }
    }

    /// Posts a notice about `subject`, replacing an older one about it.
    #[cfg(test)]
    pub fn post(&mut self, subject: &str, message: NoticeMessage, time: &str) {
        self.state.notices.retain(|n| n.subject != subject);
        self.state.notices.insert(
            0,
            Notice {
                id: self.next_id,
                subject: subject.into(),
                message,
                time: time.into(),
            },
        );
        self.next_id += 1;
        self.state.notices.truncate(NOTICE_CAPACITY);
    }

    /// Adds a speaker the example data lacks, such as a virtual output.
    #[cfg(test)]
    pub fn add_speaker(&mut self, speaker: Speaker) {
        self.state.speakers.push(speaker);
    }

    /// Has the speaker `id` ask for its password; `wrong`: it turned the
    /// last one down.
    #[cfg(test)]
    pub fn ask_password(&mut self, id: &str, wrong: bool) {
        let name = self
            .speaker_mut(id)
            .map(|s| s.name.clone())
            .unwrap_or_default();
        self.state.password_prompt = Some(PasswordPrompt {
            speaker_id: id.into(),
            name,
            wrong,
        });
    }

    /// AirPlay discovery has found nothing for a while: Connected Speakers
    /// shows its note.
    #[cfg(test)]
    pub fn set_airplay_none_found(&mut self, on: bool) {
        self.state.airplay_none_found = on;
    }

    /// Keeps only the example speakers `keep` accepts, and the groups.
    #[cfg(test)]
    pub fn retain_speakers(&mut self, keep: impl Fn(&Speaker) -> bool) {
        self.state.speakers.retain(keep);
    }

    fn speaker_mut(&mut self, id: &str) -> Option<&mut Speaker> {
        self.state.speakers.iter_mut().find(|s| s.id == id)
    }
}

#[cfg(any(not(windows), test))]
impl Backend for MockBackend {
    fn snapshot(&self) -> AppState {
        self.state.clone()
    }

    fn view_settings(&self) -> ViewSettings {
        self.view
    }

    fn set_view_settings(&mut self, settings: ViewSettings) {
        self.view = settings;
    }

    fn set_running(&mut self, on: bool) {
        self.state.running = on;
    }

    fn select_source(&mut self, id: &str) {
        if self.state.sources.iter().any(|s| s.id == id) {
            self.state.source_id = id.into();
        }
    }

    fn set_when_source_quits(&mut self, setting: WhenSourceQuits) {
        self.state.when_source_quits = setting;
    }

    fn set_master_volume(&mut self, position: f64) {
        self.state.master_volume = volume::clamp_position(position);
    }

    fn set_speaker_enabled(&mut self, id: &str, on: bool) {
        if let Some(speaker) = self.speaker_mut(id) {
            speaker.enabled = on;
        }
        // Like the Mac: turning a speaker on starts streaming.
        if on {
            self.state.running = true;
        }
    }

    fn set_speaker_volume(&mut self, id: &str, position: f64) {
        if let Some(speaker) = self.speaker_mut(id) {
            speaker.volume = volume::clamp_position(position);
        }
    }

    fn set_speaker_delay(&mut self, id: &str, ms: f64) {
        if let Some(speaker) = self.speaker_mut(id)
            && speaker.delay_ms.is_some()
        {
            speaker.delay_ms = Some(clamp_delay_ms(ms));
        }
    }

    fn set_airplay_delay(&mut self, ms: f64) {
        if !ms.is_nan() && self.state.airplay_delay_ms.is_some() {
            self.state.airplay_delay_ms = Some(ms.clamp(0.0, MAX_DELAY_MS).round());
        }
    }

    fn set_auto_connect(&mut self, id: &str, on: bool) {
        if let Some(speaker) = self.speaker_mut(id) {
            speaker.auto_connect = on;
        }
    }

    fn toggle_group(&mut self, id: &str) {
        let Some(group) = self.state.groups.iter().find(|g| g.id == id).cloned() else {
            return;
        };
        let turn_on = self.state.group_state(&group).click_turns_on();
        for member in &group.members {
            if let Some(speaker) = self.speaker_mut(member) {
                speaker.enabled = turn_on;
            }
        }
        if turn_on {
            self.state.running = true;
        }
    }

    fn new_group(&mut self, name: &str) -> Result<(), GroupError> {
        let members: Vec<String> = self
            .state
            .speakers
            .iter()
            .filter(|s| s.enabled)
            .map(|s| s.id.clone())
            .collect();
        if members.is_empty() {
            return Err(GroupError::NoActiveSpeakers);
        }
        self.state.groups.push(Group {
            id: format!("group:{}", self.next_id),
            name: name.trim().to_string(),
            members,
        });
        self.next_id += 1;
        Ok(())
    }

    fn rename_group(&mut self, id: &str, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        if let Some(group) = self.state.groups.iter_mut().find(|g| g.id == id) {
            group.name = name.into();
        }
    }

    fn delete_group(&mut self, id: &str) {
        self.state.groups.retain(|g| g.id != id);
    }

    fn set_equalizer(&mut self, settings: EqSettings) {
        self.state.equalizer = settings.quantized();
    }

    fn dismiss_notice(&mut self, id: u64) {
        self.state.notices.retain(|n| n.id != id);
    }

    fn dismiss_all_notices(&mut self) {
        self.state.notices.clear();
    }

    fn refresh_devices(&mut self) {}

    /// As Windows would report it once the fix is written.
    fn fix_device_volume(&mut self, id: &str) {
        if let Some(speaker) = self.speaker_mut(id) {
            speaker.device_volume = None;
        }
    }

    fn submit_password(&mut self, id: &str, password: &str) {
        if !password.is_empty() && self.state.password_prompt.take().is_some() {
            self.set_speaker_enabled(id, true);
        }
    }

    fn cancel_password(&mut self, id: &str) {
        self.state.password_prompt = None;
        self.set_speaker_enabled(id, false);
    }
}

/// The preset whose gains these settings have, if any (else the UI says "Custom").
pub fn matching_preset_index(settings: &EqSettings) -> Option<usize> {
    eq::PRESETS
        .iter()
        .position(|preset| preset.gains_db == settings.gains_db)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delay_field_keeps_digits_and_one_leading_sign() {
        for (typed, kept) in [
            ("150", "150"),
            ("+150", "+150"),
            ("\u{2212}250", "\u{2212}250"),
            ("-25", "-25"),
            ("1a2", "12"),
            ("15x", "15"),
            ("1-2", "12"),
            ("--5", "-5"),
            ("1.5", "15"),
            (" 7", "7"),
        ] {
            assert_eq!(delay_field_text(typed), kept, "{typed:?}");
        }
    }

    fn frame(x: i32, width: f64, maximized: bool) -> WindowFrame {
        WindowFrame {
            x,
            y: 40,
            width,
            height: 700.0,
            maximized,
        }
    }

    #[test]
    fn a_maximized_window_keeps_its_last_normal_frame_marked_maximized() {
        let normal = frame(100, 1000.0, false);
        let maximized = MainWindowShows::Maximized(frame(-8, 1920.0, false));
        assert_eq!(
            frame_to_keep(maximized, Some(normal)),
            Some(frame(100, 1000.0, true))
        );
        // Maximized before any normal frame was kept: its own frame.
        assert_eq!(
            frame_to_keep(maximized, None),
            Some(frame(-8, 1920.0, true))
        );
        // Back to normal, the flag goes.
        let shows = MainWindowShows::Normal(frame(120, 1100.0, false));
        assert_eq!(
            frame_to_keep(shows, Some(frame(100, 1000.0, true))),
            Some(frame(120, 1100.0, false))
        );
        // Hidden or minimized: what was kept stays.
        let kept = Some(frame(100, 1000.0, true));
        assert_eq!(frame_to_keep(MainWindowShows::Not, kept), kept);
    }

    #[test]
    fn a_speaker_plays_only_while_streaming() {
        assert_eq!(SpeakerState::new(false, true), SpeakerState::Off);
        assert_eq!(SpeakerState::new(true, false), SpeakerState::WillPlay);
        assert_eq!(SpeakerState::new(true, true), SpeakerState::Playing);
        assert!(SpeakerState::WillPlay.click_turns_on());
        assert!(!SpeakerState::Playing.click_turns_on());
    }

    #[test]
    fn a_speaker_on_that_cant_play_waits_instead_of_playing() {
        let mut backend = MockBackend::new();
        backend.set_running(true);
        let mut state = backend.snapshot();
        let mut kitchen = state.speaker("airplay:kitchen").unwrap().clone();
        kitchen.waiting = Some(Waiting::Reconnecting);
        assert_eq!(
            state.speaker_state(&kitchen),
            SpeakerState::Waiting(Waiting::Reconnecting)
        );
        assert!(
            !SpeakerState::Waiting(Waiting::ForDevice).click_turns_on(),
            "it is on, so a click turns it off"
        );
        // Without streaming it just plays once streaming starts; off is off.
        state.running = false;
        assert_eq!(state.speaker_state(&kitchen), SpeakerState::WillPlay);
        state.running = true;
        kitchen.enabled = false;
        assert_eq!(state.speaker_state(&kitchen), SpeakerState::Off);
    }

    #[test]
    fn this_pc_comes_first_then_the_rest_by_name() {
        let state = MockBackend::new().snapshot();
        let (this_pc, connected) = state.layout();
        let names = |list: &[&Speaker]| list.iter().map(|s| s.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&this_pc), ["Speakers", "Headphones"]);
        assert_eq!(
            names(&connected),
            [
                "Bedroom",
                "Desk Speaker",
                "Headset",
                "Kitchen",
                "Living Room"
            ]
        );
    }

    #[test]
    fn virtual_outputs_are_listed_only_when_shown_or_on() {
        let speaker = |kind, enabled| Speaker {
            id: "local:x".into(),
            name: "X".into(),
            kind,
            enabled,
            volume: 60.0,
            delay_ms: Some(0.0),
            auto_connect: false,
            availability: Availability::Open,
            paired: false,
            device_volume: None,
            waiting: None,
            cast: None,
            group: None,
        };
        assert!(!speaker(SpeakerKind::Virtual, false).is_listed(false));
        assert!(speaker(SpeakerKind::Virtual, false).is_listed(true));
        assert!(speaker(SpeakerKind::Virtual, true).is_listed(false));
        for kind in [
            SpeakerKind::BuiltIn,
            SpeakerKind::Usb,
            SpeakerKind::Bluetooth,
        ] {
            assert!(speaker(kind, false).is_listed(false), "{kind:?}");
        }
    }

    #[test]
    fn turning_a_speaker_on_starts_streaming() {
        let mut backend = MockBackend::new();
        backend.set_speaker_enabled("local:desk", true);
        let state = backend.snapshot();
        assert!(state.running);
        assert_eq!(
            state.speaker_state(state.speaker("local:desk").unwrap()),
            SpeakerState::Playing
        );
    }

    #[test]
    fn a_group_is_on_when_all_its_present_members_are() {
        let mut backend = MockBackend::new();
        let group = backend.snapshot().groups[0].clone();
        assert_eq!(backend.snapshot().group_state(&group), SpeakerState::Off);
        backend.toggle_group(&group.id);
        let state = backend.snapshot();
        assert_eq!(state.group_state(&group), SpeakerState::Playing);
        backend.toggle_group(&group.id);
        let state = backend.snapshot();
        assert!(state.available_members(&group).all(|s| !s.enabled));
    }

    #[test]
    fn a_group_waits_when_all_its_present_members_wait() {
        let mut backend = MockBackend::new();
        let group = backend.snapshot().groups[0].clone();
        backend.toggle_group(&group.id);
        backend.set_running(true);
        let mut state = backend.snapshot();
        let wait = |state: &mut AppState, id: &str, waiting| {
            state
                .speakers
                .iter_mut()
                .find(|s| s.id == id)
                .unwrap()
                .waiting = waiting;
        };
        wait(&mut state, "airplay:kitchen", Some(Waiting::Reconnecting));
        assert_eq!(
            state.group_state(&group),
            SpeakerState::Playing,
            "one member still plays"
        );
        wait(&mut state, "airplay:living", Some(Waiting::Reconnecting));
        assert_eq!(
            state.group_state(&group),
            SpeakerState::Waiting(Waiting::Reconnecting),
            "every present member waits"
        );
        state.speakers.retain(|s| s.id != "airplay:living");
        assert_eq!(
            state.group_state(&group),
            SpeakerState::Waiting(Waiting::Reconnecting),
            "a member that went doesn't count"
        );
        assert!(
            !state.group_state(&group).click_turns_on(),
            "a click turns a waiting group off"
        );
        state.running = false;
        assert_eq!(state.group_state(&group), SpeakerState::WillPlay);
    }

    #[test]
    fn a_new_group_needs_a_speaker_that_is_on() {
        let mut backend = MockBackend::new();
        for id in ["local:speakers", "airplay:kitchen"] {
            backend.set_speaker_enabled(id, false);
        }
        assert_eq!(
            backend.new_group("Office"),
            Err(GroupError::NoActiveSpeakers)
        );
        backend.set_speaker_enabled("local:desk", true);
        assert_eq!(backend.new_group(" Office "), Ok(()));
        let group = backend.snapshot().groups.last().cloned().unwrap();
        assert_eq!(group.name, "Office");
        assert_eq!(group.members, ["local:desk"]);
    }

    #[test]
    fn delays_are_whole_milliseconds_within_the_limit() {
        assert_eq!(parse_delay_ms("120"), Some(120.0));
        assert_eq!(parse_delay_ms(" 99,6 "), Some(100.0));
        assert_eq!(parse_delay_ms("-5"), Some(-5.0));
        assert_eq!(parse_delay_ms("999999"), Some(MAX_DELAY_MS));
        assert_eq!(parse_delay_ms(""), None);
        assert_eq!(parse_delay_ms("12a"), None);
        assert_eq!(parse_delay_ms("inf"), None);
        let mut backend = MockBackend::new();
        backend.set_speaker_delay("airplay:kitchen", 50.0);
        let state = backend.snapshot();
        assert_eq!(state.speaker("airplay:kitchen").unwrap().delay_ms, None);
    }

    #[test]
    fn an_offset_takes_a_leading_plus_or_minus_and_other_text_is_ignored() {
        assert_eq!(parse_delay_ms("+150"), Some(150.0));
        assert_eq!(parse_delay_ms("−250"), Some(-250.0));
        assert_eq!(parse_delay_ms("-250"), Some(-250.0));
        assert_eq!(parse_delay_ms(" −99,6 "), Some(-100.0));
        assert_eq!(parse_delay_ms("−3000"), Some(-2000.0), "clamped");
        assert_eq!(parse_delay_ms("+"), None);
        assert_eq!(parse_delay_ms("−"), None);
        assert_eq!(parse_delay_ms("+-5"), None);
        assert_eq!(parse_delay_ms("1−2"), None);
        assert_eq!(clamp_delay_ms(-2400.0), -2000.0);
        assert_eq!(clamp_delay_ms(25_000.0), 20_000.0);
    }

    #[test]
    fn a_newer_notice_about_the_same_subject_replaces_the_older_one() {
        let quit = |source: &str| NoticeMessage::SourceQuit {
            source: source.into(),
        };
        let mut backend = MockBackend::new();
        backend.post("source", quit("first"), "09.40");
        backend.post("start", NoticeMessage::NoSpeaker, "09.41");
        backend.post("source", quit("second"), "09.42");
        let messages: Vec<_> = backend
            .snapshot()
            .notices
            .iter()
            .map(|n| n.message.clone())
            .collect();
        assert_eq!(messages, [quit("second"), NoticeMessage::NoSpeaker]);
        for n in 0..10 {
            backend.post(&format!("s{n}"), NoticeMessage::CaptureLost, "09.43");
        }
        assert_eq!(backend.snapshot().notices.len(), NOTICE_CAPACITY);
    }

    #[test]
    fn the_equalizer_is_quantized_and_matches_presets() {
        let mut backend = MockBackend::new();
        let mut settings = EqSettings::default();
        settings.gains_db[0] = 3.3;
        backend.set_equalizer(settings);
        assert_eq!(backend.snapshot().equalizer.gains_db[0], 3.5);
        assert_eq!(matching_preset_index(&EqSettings::default()), Some(0));
        assert_eq!(matching_preset_index(&backend.snapshot().equalizer), None);
    }
}
