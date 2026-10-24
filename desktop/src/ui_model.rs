//! What the interface shows and what it can ask for, independent of how the
//! audio work is done.
//!
//! The UI reads an [`AppState`] snapshot from a [`Backend`] and calls the
//! backend's methods for every user action, then reads a new snapshot. On
//! Windows the backend is `crate::adapter::AudioAdapter` over the WASAPI audio
//! backend. [`MockBackend`], built only with the `mock-backend` feature, stands
//! in with example data for UI work: off Windows always, on Windows for
//! `AUDIOSYNC_PREVIEW_ZONES`. Nothing here makes a sound or opens an audio
//! device.

use audiosync_core::eq::{self, CurveMatch, EqSettings, UserCurve};
use audiosync_core::policy::device_volume::Hint;
use audiosync_core::policy::presets::{self as policy_presets, Preset, Scene, SpeakerNow};
use audiosync_core::policy::source_quit::WhenSourceQuits;
use audiosync_core::policy::zones::{Zone, Zones};
#[cfg(any(test, feature = "mock-backend"))]
use audiosync_core::volume;

#[cfg(any(test, feature = "mock-backend"))]
use crate::speaker_names;
use audiosync_core::calibrate_all::{AirPlayMoves, AirPlayShared, Class, EndReason, RowState};

pub use crate::audio::backend::{CastMembers, InTarget};
use crate::audio::calibration::{Failure, Phase};
pub use crate::audio::outputs::MicKind;

pub use crate::settings::Appearance;

/// The largest offset the sync controls accept, in milliseconds (the macOS
/// app's limit too).
pub const MAX_DELAY_MS: f64 = 20_000.0;

/// The smallest offset the sync controls accept, in milliseconds: a speaker
/// that sounds up to two seconds late.
pub const MIN_DELAY_MS: f64 = -2000.0;

/// The most notices kept at once; the oldest drops off past it.
#[cfg(any(test, feature = "mock-backend"))]
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
    /// Every app except those named; the UI words it around the names.
    AppsExcept,
}

/// How long the AirPlay speakers make every speaker wait, as the live input
/// note names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AirPlayWait {
    /// In tenths of a second.
    Tenths(u32),
    /// No AirPlay sender reports its delay yet.
    Unknown,
}

/// An app the several-apps list offers, by executable file name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MixApp {
    pub id: String,
    pub name: String,
    /// Checked in the mode the list shows.
    pub checked: bool,
}

/// Something AudioSync can capture.
#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    pub id: String,
    pub name: String,
    pub kind: SourceKind,
    /// An input device's channels, which offer pairs above two; 0 for
    /// anything else and for a device that doesn't say.
    pub channels: u16,
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
        all(not(os_backend), not(test)),
        expect(dead_code, reason = "only the Windows backend pairs")
    )]
    Pairing,
    /// It was switched on while another Apple TV has the PIN prompt; its
    /// pairing gets the prompt once that one ends.
    #[cfg_attr(
        all(not(os_backend), not(test)),
        expect(dead_code, reason = "only the Windows backend pairs")
    )]
    WaitingToPair,
    /// It needs a kind of pairing AudioSync for Windows can't do yet.
    #[cfg_attr(
        all(not(os_backend), not(test)),
        expect(dead_code, reason = "only the Windows backend lists real receivers")
    )]
    PairingNotYet,
    /// Only AirPlay 2 can play it, which AudioSync for Windows doesn't speak
    /// yet.
    AirPlay2NotYet,
    /// It wants an encryption, codec or format AudioSync can't send.
    #[cfg_attr(
        all(not(os_backend), not(test)),
        expect(dead_code, reason = "only the Windows backend lists real receivers")
    )]
    Unsupported,
}

/// What went wrong with an AirPlay speaker, as a notice words it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    all(not(os_backend), not(test)),
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
    /// The name the user gave it, else its system name.
    pub name: String,
    /// The name Windows or AirPlay discovery reports.
    pub system_name: String,
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
    /// An AirPlay speaker whose password AudioSync holds until it quits,
    /// which the user can forget.
    pub holds_password: bool,
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
    /// A local output microphone calibration leaves out by its kind.
    pub mic_left_out: Option<MicKind>,
    /// The stereo pair or home theater this AirPlay speaker plays only as
    /// part of, which keeps it off by itself; by the target's speaker ID.
    pub in_target: Option<InTarget>,
}

/// A speaker hidden with Hide Speaker, as Speakers > Hidden Speakers lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct HiddenSpeaker {
    /// The ID its row had, its speaker key.
    pub id: String,
    /// Its name now while it is listed, else the name it had when hidden.
    pub name: String,
    /// The kind its row showed when it was hidden, for while it isn't listed.
    pub kind_text: String,
    /// Its kind while it is listed; `None` while it isn't.
    pub present: Option<SpeakerKind>,
}

/// A zone member that isn't listed and isn't hidden: its row says it isn't
/// found and offers only Move to Zone and Remove from Zone.
#[derive(Clone, Debug, PartialEq)]
pub struct AbsentMember {
    /// Its speaker key.
    pub id: String,
    /// The name the user gave it, else the one it had when last listed in
    /// this run; empty when neither is known.
    pub name: String,
    pub kind: SpeakerKind,
}

/// A row under a zone's header (spec S245): every member but the hidden ones,
/// so the rows and the header's count agree.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ZoneRow<'a> {
    Listed(&'a Speaker),
    /// Not listed right now, by speaker key.
    NotFound(&'a str),
}

/// What a zone member's row says in place of its own note (spec S246, S247).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZoneNote<'a> {
    /// A stereo pair or home theater while AirPlay 2 is off.
    NeedsAirPlay2,
    /// It plays only as part of this stereo pair or home theater, by name.
    InTarget(&'a str),
    NotFound,
    /// A Cast speaker the zone's click left off: one Cast speaker plays at a
    /// time.
    CastStayedOff,
}

/// A Google Cast speaker's own state. Its delay is the measured offset the
/// user sets, since Cast can't be kept in sync automatically.
#[derive(Clone, Debug, PartialEq)]
pub struct CastRow {
    pub state: CastState,
    /// The speaker reports itself muted.
    pub muted: bool,
    /// The speaker's volume can't be changed from here.
    pub volume_fixed: bool,
    /// How far the volume keys move it, 0 to 100 (the device's own step).
    pub volume_step: f64,
    /// Set for a Cast group, which plays as one speaker.
    pub group: Option<CastGroup>,
    /// The user added it by address (spec C18): its row says so, and its
    /// menu removes it.
    pub added_by_address: bool,
}

/// A Google Cast group and what its row knows of its members.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CastGroup {
    pub members: CastMembers,
}

/// Where a Google Cast speaker that is on is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    all(not(os_backend), not(test)),
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
    all(not(os_backend), not(test)),
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
    all(not(os_backend), not(test)),
    expect(dead_code, reason = "only the Windows backend posts notices")
)]
pub enum CastProblem {
    /// Another Cast speaker is on; one plays at a time.
    Busy,
    NotResponding,
    /// It turned the stream down.
    Refused,
    /// It couldn't fetch the stream from this PC.
    Blocked,
    /// Another app started casting to it.
    TakenOver,
    ConnectionLost,
}

/// Why a speaker that is on can't play right now. Its row says so instead
/// of showing it as playing; it stays on and plays again by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    all(not(os_backend), not(test)),
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
    all(not(os_backend), not(test)),
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
    /// The reference output: the one local output with no offset of its own.
    pub fn is_reference(&self) -> bool {
        self.delay_ms.is_none()
            && !matches!(
                self.kind,
                SpeakerKind::AirPlay | SpeakerKind::AirPlay2 | SpeakerKind::Cast
            )
    }

    /// Whether the speaker lists show it. A virtual output (a loopback
    /// driver, a voice changer, a meeting app's device) is listed only while
    /// Show Virtual Devices is on or while it is turned on. The main window
    /// also keeps one listed once it was shown (`ListedVirtual` in
    /// src/view.rs), so turning it off doesn't remove its row.
    pub fn is_listed(&self, show_virtual: bool) -> bool {
        show_virtual || self.enabled || self.kind != SpeakerKind::Virtual
    }
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

/// A source other than the whole system, as a notice names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceName {
    /// An app or an input device, by the name shown for it.
    Named(String),
    /// Several apps, by name; the UI joins the list.
    Apps(Vec<String>),
    /// Every app except the listed ones; the UI words it around the list.
    AllAppsExcept(Vec<String>),
}

/// What a notice says. The UI words it (the notice texts in
/// `ui/strings.slint`), so no backend builds user-facing text.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(
    all(not(os_backend), not(test)),
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
    /// A zone or preset the web remote turned on left this speaker off: it
    /// asks for a code, which only the PC can answer.
    PairFirst { speaker: String },
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
    /// A member of the stereo pair or home theater group `target` stays off
    /// by itself while the target is listed.
    AirPlayPlaysInGroup { speaker: String, target: String },
    /// A member of a stereo pair or home theater group asks for a password,
    /// so the target is off.
    AirPlayMemberNeedsPassword { target: String, member: String },
    /// AudioSync's AirPlay sender stopped after an internal error or
    /// couldn't start, so AirPlay speakers aren't listed or playing. The
    /// notice offers Retry ([`Backend::retry_airplay`]).
    AirPlayStopped,
    /// Another app holds PTP port 319 or 320, so AirPlay speakers that need
    /// PTP timing can't play. An AirPlay speaker that plays clears it.
    AirPlayPtpPortsTaken,
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
    /// is heard twice. `source` names the app, or is `None` for the whole
    /// system. `mute` names the outputs AudioSync doesn't play to, which the
    /// user may mute; `shared` the ones AudioSync plays to as well, where
    /// only sending the source elsewhere helps. `default_output` is the one
    /// of them that counts because it is Windows' default output (the whole
    /// system only); any other plays the source because an app plays there.
    HeardTwice {
        source: Option<SourceName>,
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
    /// File > AirPlay 2 (Experimental) was turned on (`on`) or off, which
    /// applies once AudioSync is quit and opened again.
    AirPlay2NextLaunch { on: bool },
    /// The web remote listens while a connected network is Public, where
    /// anyone on it can reach the pairing page.
    RemoteOnPublicNetwork,
    /// A Google Cast speaker can't play, so it is off.
    CastCantPlay {
        speaker: String,
        problem: CastProblem,
    },
    /// The AirPlay speakers can't wait long enough for the Google Cast
    /// speaker that is on, so they play about `tenths` tenths of a second
    /// ahead of it. Clears itself once they keep up again.
    AirPlayAheadOfCast { speaker: String, tenths: u32 },
    /// The saved groups became zones at this launch (spec S248); posted once.
    GroupsBecameZones,
    /// A preset recall left stored members off (spec S265), each by its
    /// shown name with why; `nothing_changed`: none could go on, so the
    /// recall changed nothing.
    PresetLeftOff {
        preset: String,
        left_off: Vec<(String, PresetLeftOff)>,
        nothing_changed: bool,
    },
}

/// Why a preset's member stayed off at recall (spec S252, S265).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PresetLeftOff {
    NotFound,
    Hidden,
    NeedsAirPlay2,
    /// Plays as part of the stereo pair or home theater with this shown name.
    InTarget(String),
    /// A Google Cast speaker another app casts to.
    Busy,
    /// The recall switched it on, and it failed to start (spec S266).
    CantPlay,
}

/// At most this many presets: the settings file keeps no more (spec S251).
pub const MAX_PRESETS: usize = crate::settings::MAX_ENTRIES;

/// The speakers on now as a preset sees them (spec S251, S253): list
/// order, the reference output left out, no own volume for a fixed-volume
/// Cast speaker. A hidden speaker is never listed, so never captured.
pub fn speakers_on_now(state: &AppState) -> Vec<SpeakerNow> {
    state
        .speakers
        .iter()
        .filter(|speaker| speaker.enabled && !speaker.is_reference())
        .map(|speaker| SpeakerNow {
            key: speaker.id.clone(),
            volume: (!speaker.cast.as_ref().is_some_and(|cast| cast.volume_fixed))
                .then_some(speaker.volume),
        })
        .collect()
}

/// The preset that `state` plays exactly, the first in list order (spec
/// S253).
pub fn current_preset(presets: &[Preset], state: &AppState) -> Option<String> {
    let on = speakers_on_now(state);
    let zones = Zones::from_stored(state.zones.clone());
    let scene = Scene {
        on: &on,
        zones: &zones,
        master: state.master_volume,
        gains_db: state.equalizer.gains_db,
    };
    policy_presets::current(presets, &scene).map(|preset| preset.id.clone())
}

/// What `state` plays now as preset `id` named `name` (spec S251); `None`
/// with no speaker on.
pub fn capture_preset(id: &str, name: &str, state: &AppState) -> Option<Preset> {
    let on = speakers_on_now(state);
    if on.is_empty() {
        return None;
    }
    let zones = Zones::from_stored(state.zones.clone());
    let scene = Scene {
        on: &on,
        zones: &zones,
        master: state.master_volume,
        gains_db: state.equalizer.gains_db,
    };
    Some(Preset::capture(id, name, &scene))
}

/// Fills in `state`'s preset list and the preset it plays now (spec S264).
pub fn show_presets(state: &mut AppState, presets: &[Preset]) {
    state.current_preset = current_preset(presets, state);
    state.presets = presets
        .iter()
        .map(|preset| PresetItem {
            id: preset.id.clone(),
            name: preset.name.clone(),
            current: state.current_preset.as_deref() == Some(preset.id.as_str()),
            members: preset
                .members
                .iter()
                .map(|member| member.key.clone())
                .collect(),
        })
        .collect();
}

/// An ID no preset in `presets` has.
pub fn new_preset_id(presets: &[Preset]) -> String {
    (presets.len() + 1..)
        .map(|n| format!("preset:{n}"))
        .find(|id| presets.iter().all(|preset| preset.id != *id))
        .unwrap_or_default()
}

/// The preset notice after a recall of `preset` (spec S265); `None` when
/// the recall left no member off, so the notice clears.
pub fn preset_notice(
    preset: &str,
    left_off: Vec<(String, PresetLeftOff)>,
    nothing_changed: bool,
) -> Option<NoticeMessage> {
    (nothing_changed || !left_off.is_empty()).then(|| NoticeMessage::PresetLeftOff {
        preset: preset.to_owned(),
        left_off,
        nothing_changed,
    })
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
    /// The channel pair saved for the chosen input device, as an index (1
    /// for channels 3-4); 0 for 1-2 and for any other source.
    pub input_pair: u16,
    /// The several-apps list: the apps to check.
    pub mix_apps: Vec<MixApp>,
    /// The list shows "every app except the checked ones", not "only them".
    pub mix_all_except: bool,
    /// 0 to 100.
    pub master_volume: f64,
    /// The speakers listed; a hidden one never is.
    pub speakers: Vec<Speaker>,
    /// The hidden speakers, by name (case-insensitively), then by ID.
    pub hidden: Vec<HiddenSpeaker>,
    /// The zones (spec S245), in order.
    pub zones: Vec<Zone>,
    /// The zone members that aren't listed and aren't hidden.
    pub absent: Vec<AbsentMember>,
    pub equalizer: EqSettings,
    /// The equalizer curves the user saved, in the picker's order (spec S270).
    pub equalizer_curves: Vec<UserCurve>,
    /// The speaker presets in list order (spec S259), the current one
    /// marked (spec S253).
    pub presets: Vec<PresetItem>,
    /// The ID of the preset that plays exactly now (spec S253, S264).
    pub current_preset: Option<String>,
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
    /// AirPlay discovery runs and has listed no speaker yet, short of
    /// `airplay_none_found`: the flyout says it is looking.
    pub airplay_searching: bool,
    /// What a session does when the app it captures quits (spec S227).
    pub when_source_quits: WhenSourceQuits,
    /// The speaker list's note about Google Cast discovery.
    pub cast_list: CastList,
    /// A Google Cast sender runs, so Add by Address… can list a speaker.
    pub cast_by_address: bool,
    /// The microphone calibration a row shows.
    pub calibration: CalibrationState,
    /// Calibrate All Speakers, in its own card.
    pub calibrate_all: CalibrateAllState,
}

/// A speaker preset as the windows and the web remote list it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresetItem {
    pub id: String,
    pub name: String,
    /// The speakers play it exactly now, the first such preset in list order.
    pub current: bool,
    /// The stored members' keys, in order.
    pub members: Vec<String>,
}

/// Calibrate All Speakers (spec S235 to S240): where a run stands and the
/// results of the last one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CalibrateAllState {
    pub phase: AllPhase,
    /// Why a run can't start now; `None` when the button may try.
    pub blocked: Option<Failure>,
    /// A one-speaker calibration holds the outputs.
    pub other_running: bool,
    /// The results, while they still describe the stored delays.
    pub sheet: Option<SheetState>,
    /// Counts each ending, Apply and Undo, so Narrator hears each once.
    pub endings: u64,
    /// The outputs that are on but left out by their kind.
    pub left_out: Vec<LeftOutRow>,
    /// The zone a Calibrate Zone run measures or measured (spec S255).
    pub zone: Option<ZoneSheet>,
}

/// What a Calibrate Zone sheet says besides Calibrate All's (spec S255).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ZoneSheet {
    /// "Bring the computer into <name>".
    pub name: String,
    /// The sheet's rows that are members that were off: "Off".
    pub off_rows: Vec<usize>,
    /// Whom the shared AirPlay delay moves besides the zone.
    pub airplay_moves: AirPlayMoves,
}

/// An output that is on, which Calibrate All leaves out by its kind unless
/// the user chose Include Anyway.
#[derive(Clone, Debug, PartialEq)]
pub struct LeftOutRow {
    /// Its endpoint ID.
    pub id: String,
    pub name: String,
    pub kind: MicKind,
    pub included: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum AllPhase {
    #[default]
    Idle,
    Running(AllProgress),
    /// It couldn't start, or the microphone or the capture stopped it.
    Failed(Failure),
    /// Measure Again couldn't start: the reference or the named row's
    /// speaker isn't playing.
    CannotMeasureAgain(String),
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

    /// The zone that holds speaker `id`, if one does.
    pub fn zone_of(&self, id: &str) -> Option<&Zone> {
        self.zones
            .iter()
            .find(|zone| zone.members.iter().any(|member| member == id))
    }

    /// The members of `zone` that are listed right now, in zone order.
    pub fn zone_members<'a>(&'a self, zone: &'a Zone) -> impl Iterator<Item = &'a Speaker> {
        zone.members.iter().filter_map(|id| self.speaker(id))
    }

    /// The rows under `zone`'s header, in zone order: a listed member, the
    /// virtual outputs View hides included, or one not found; a hidden
    /// member has none.
    pub fn zone_rows<'a>(&'a self, zone: &'a Zone) -> Vec<ZoneRow<'a>> {
        zone.members
            .iter()
            .filter(|id| !self.hidden.iter().any(|hidden| hidden.id == **id))
            .map(|id| {
                self.speaker(id)
                    .map_or(ZoneRow::NotFound(id.as_str()), ZoneRow::Listed)
            })
            .collect()
    }

    /// What a zone header counts, from its rows.
    pub fn zone_counts(&self, zone: &Zone) -> ZoneCounts {
        let rows = self.zone_rows(zone);
        let on: Vec<&Speaker> = rows
            .iter()
            .filter_map(|row| match row {
                ZoneRow::Listed(speaker) if speaker.enabled => Some(*speaker),
                _ => None,
            })
            .collect();
        ZoneCounts {
            total: rows.len(),
            present: rows
                .iter()
                .filter(|row| matches!(row, ZoneRow::Listed(_)))
                .count(),
            on: on.len(),
            waiting: if self.running {
                on.iter().filter(|s| s.waiting.is_some()).count()
            } else {
                0
            },
        }
    }

    /// Whether zone `zone`'s Calibrate Zone… can be chosen: a member is
    /// here and no calibration holds the outputs, since a start then does
    /// nothing (spec S255).
    pub fn offers_zone_run(&self, zone: &Zone) -> bool {
        self.zone_counts(zone).present > 0 && !self.calibration.busy
    }

    /// The note on `row` under `zone`, if it has one. The Cast note needs
    /// another member on: the click plan switches on one Cast member at most.
    pub fn zone_note<'a>(&self, zone: &Zone, row: ZoneRow<'a>) -> Option<ZoneNote<'a>> {
        let speaker = match row {
            ZoneRow::NotFound(_) => return Some(ZoneNote::NotFound),
            ZoneRow::Listed(speaker) => speaker,
        };
        if speaker.group.is_some() && speaker.availability == Availability::AirPlay2NotYet {
            return Some(ZoneNote::NeedsAirPlay2);
        }
        if let Some(target) = &speaker.in_target {
            return Some(ZoneNote::InTarget(&target.name));
        }
        let other_on = || {
            self.zone_members(zone)
                .any(|member| member.enabled && member.id != speaker.id)
        };
        (speaker.kind == SpeakerKind::Cast
            && !speaker.enabled
            && speaker.availability == Availability::Open
            && other_on())
        .then_some(ZoneNote::CastStayedOff)
    }

    /// The name and kind of zone member `id` while it isn't listed.
    pub fn absent_member(&self, id: &str) -> Option<&AbsentMember> {
        self.absent.iter().find(|member| member.id == id)
    }

    /// A zone is on when a click would switch it off (spec S246): each
    /// present member it switches on is on, which is every one but those
    /// playing as part of a stereo pair or home theater, and one Cast
    /// member when it has any. Playing, it waits when each of those waits.
    pub fn zone_state(&self, zone: &Zone) -> SpeakerState {
        let switched: Vec<&Speaker> = self
            .zone_members(zone)
            .filter(|s| s.in_target.is_none())
            .collect();
        let (cast, others): (Vec<&Speaker>, Vec<&Speaker>) =
            switched.iter().partition(|s| s.kind == SpeakerKind::Cast);
        let cast_on: Vec<&Speaker> = cast.iter().copied().filter(|s| s.enabled).collect();
        let on = !switched.is_empty()
            && others.iter().all(|s| s.enabled)
            && (cast.is_empty() || !cast_on.is_empty());
        match SpeakerState::new(on, self.running) {
            SpeakerState::Playing => {
                let waiting: Option<Vec<Waiting>> = others
                    .iter()
                    .chain(cast_on.iter().take(1))
                    .map(|s| s.waiting)
                    .collect();
                waiting
                    .and_then(|w| w.first().copied())
                    .map_or(SpeakerState::Playing, SpeakerState::Waiting)
            }
            state => state,
        }
    }

    /// Whether a hidden speaker that is present belongs under This PC
    /// (`this_pc`) or Connected Speakers: that section's empty text then
    /// says its speakers are hidden.
    pub fn hidden_in(&self, this_pc: bool) -> bool {
        self.hidden.iter().any(|hidden| {
            hidden
                .present
                .is_some_and(|kind| kind.is_this_pc() == this_pc)
        })
    }

    /// Whether the speakers present are all hidden, at least one of them.
    pub fn all_hidden(&self) -> bool {
        self.speakers.is_empty() && self.hidden.iter().any(|hidden| hidden.present.is_some())
    }

    /// Whether the tray flyout lists only the PC's own speakers because
    /// every connected speaker present is hidden and there is no zone.
    pub fn connected_all_hidden(&self) -> bool {
        self.zones.is_empty()
            && !self.speakers.is_empty()
            && self.speakers.iter().all(|s| s.kind.is_this_pc())
            && self.hidden_in(false)
    }

    /// Speakers switched on right now.
    pub fn active_speakers(&self) -> usize {
        self.speakers.iter().filter(|s| s.enabled).count()
    }

    /// The note under the source while an input device plays and an AirPlay
    /// speaker is on: every speaker then waits the AirPlay delay, which is
    /// too late for live sound. `None` hides the note.
    pub fn live_input_airplay_wait(&self) -> Option<AirPlayWait> {
        let input = self
            .sources
            .iter()
            .any(|s| s.id == self.source_id && s.kind == SourceKind::Input);
        let airplay_on = self
            .speakers
            .iter()
            .any(|s| s.enabled && matches!(s.kind, SpeakerKind::AirPlay | SpeakerKind::AirPlay2));
        if !input || !airplay_on {
            return None;
        }
        Some(match self.airplay_delay_ms {
            Some(ms) => AirPlayWait::Tenths((ms.max(0.0) / 100.0).round() as u32),
            None => AirPlayWait::Unknown,
        })
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

/// Why Add by Address… turned an address down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CastAddressError {
    /// It isn't a host name or IPv4 address with an optional port.
    Invalid,
    /// It doesn't resolve, or nothing answers on its port.
    NotFound,
}

/// An address Add by Address… took, with the check of whether something
/// answers there, which can wait up to 3 s and so runs off the UI thread.
#[derive(Clone, Debug)]
pub struct CastAddressCheck {
    pub address: crate::audio::cast_address::CastAddress,
    pub answers: fn(&crate::audio::cast_address::CastAddress) -> bool,
}

impl CastAddressCheck {
    /// Whether something answers at the address.
    pub fn run(&self) -> bool {
        (self.answers)(&self.address)
    }
}

/// What a zone header counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZoneCounts {
    /// Its members but the hidden ones.
    pub total: usize,
    /// Those listed right now.
    pub present: usize,
    /// Those switched on.
    pub on: usize,
    /// Those on that can't play right now, while streaming is on.
    pub waiting: usize,
}

/// Why a new zone wasn't made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZoneError {
    /// No speaker is on, so the zone would be empty.
    NoActiveSpeakers,
}

/// Why Save as Preset… or Update to Current Settings saved nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresetError {
    /// No speaker is on, so the preset would hold none (spec S262).
    NoSpeakerOn,
    /// There are [`MAX_PRESETS`] already (spec S251).
    Full,
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
    /// Plays channel pair `pair` (1 for channels 3-4) of the chosen input
    /// device, and keeps it for that device.
    fn select_input_pair(&mut self, pair: u16);
    /// Checks or unchecks an app of the several-apps list, by executable.
    fn toggle_mix_app(&mut self, executable: &str);
    /// Switches the several-apps list to "all except" (true) or "only" (false).
    fn set_mix_all_except(&mut self, on: bool);
    fn set_master_volume(&mut self, volume: f64);
    fn set_speaker_enabled(&mut self, id: &str, on: bool);
    fn set_speaker_volume(&mut self, id: &str, volume: f64);
    fn set_speaker_delay(&mut self, id: &str, ms: f64);
    /// Mutes or unmutes a Google Cast speaker on the speaker itself.
    fn set_speaker_muted(&mut self, _id: &str, _muted: bool) {}
    /// The Resync button of a Google Cast speaker: it restarts its stream so
    /// it plays at its set offset again.
    fn resync_speaker(&mut self, _id: &str) {}
    /// Add by Address…, first half: takes `text`, a host name or IPv4
    /// address with an optional port (spec C18), and returns the check that
    /// the caller runs off the UI thread.
    fn check_cast_address(&mut self, _text: &str) -> Result<CastAddressCheck, CastAddressError> {
        Err(CastAddressError::NotFound)
    }
    /// Add by Address…, second half: once the check `answered`, lists the
    /// Google Cast speaker at `address` and keeps it for the next launch.
    fn add_checked_cast_address(
        &mut self,
        _address: crate::audio::cast_address::CastAddress,
        _answered: bool,
    ) -> Result<(), CastAddressError> {
        Err(CastAddressError::NotFound)
    }
    /// Both halves of Add by Address… in a row, on the calling thread.
    #[cfg(test)]
    fn add_cast_address(&mut self, text: &str) -> Result<(), CastAddressError> {
        let check = self.check_cast_address(text)?;
        let answered = check.run();
        self.add_checked_cast_address(check.address, answered)
    }
    /// Remove, on the row of a speaker added by address.
    fn remove_cast_address(&mut self, _id: &str) {}
    /// The user entered `ms` in the AirPlay delay field.
    fn set_airplay_delay(&mut self, ms: f64);
    fn set_auto_connect(&mut self, id: &str, on: bool);
    /// Gives speaker `id` the name `name`; an empty one brings back its
    /// system name (spec S228).
    fn rename_speaker(&mut self, id: &str, name: &str);
    /// Hide Speaker on speaker `id`'s row: a speaker that is on goes off
    /// first, as its own switch turns it off, then it leaves every list.
    /// What was set for it stays. `kind` is the kind its row shows, kept so
    /// Hidden Speakers can name it while it isn't listed.
    fn hide_speaker(&mut self, _id: &str, _kind: &str) {}
    /// A Hidden Speakers entry: speaker `id` is listed again, off.
    fn unhide_speaker(&mut self, _id: &str) {}
    /// Switches zone `id`'s members as the core's click plan says (spec
    /// S246, S247).
    fn toggle_zone(&mut self, id: &str);
    /// Makes a zone of the speakers that are on now, each leaving the zone
    /// that held it (S245). The UI names it ("Zone 2") when the user leaves
    /// the name empty.
    fn new_zone(&mut self, name: &str) -> Result<(), ZoneError>;
    /// New Zone… in a speaker's menu: a zone of speaker `speaker_id` alone.
    fn new_zone_with(&mut self, speaker_id: &str, name: &str);
    /// Renames zone `id`; a blank name keeps the old one (S150).
    fn rename_zone(&mut self, id: &str, name: &str);
    /// Deletes zone `id`; its members go back to the plain lists.
    fn delete_zone(&mut self, id: &str);
    /// Moves speaker `speaker_id` into zone `zone_id`, out of the one that
    /// held it.
    fn move_to_zone(&mut self, speaker_id: &str, zone_id: &str);
    /// Takes speaker `speaker_id` out of its zone.
    fn remove_from_zone(&mut self, speaker_id: &str);
    /// Sets zone `id`'s volume, 0 to 100, which scales each member's own
    /// volume (spec S249).
    fn set_zone_volume(&mut self, id: &str, volume: f64);
    /// Use … Instead in a zone's menu: the stereo pair or home theater that
    /// member `speaker_id` plays as part of takes its place in the zone.
    fn use_target_instead(&mut self, speaker_id: &str);
    /// A zone click from the web remote: the members in `keep_off`, which
    /// ask for a code, stay off with a notice each (spec S257).
    fn toggle_zone_from_phone(&mut self, id: &str, keep_off: &[String]);
    /// A recall from the web remote, `keep_off` as for a zone click.
    fn recall_preset_from_phone(&mut self, id: &str, keep_off: &[String]);
    fn set_equalizer(&mut self, settings: EqSettings);
    /// Replaces the saved equalizer curves; the backend keeps them as the
    /// core cleans a stored list (spec S270).
    fn set_equalizer_curves(&mut self, curves: Vec<UserCurve>);
    /// Save as Preset…: what plays now becomes preset `name`, last in the
    /// list (spec S251, S262). The UI names it ("Preset 2") when the user
    /// leaves the name empty.
    fn save_preset(&mut self, name: &str) -> Result<(), PresetError>;
    /// Renames preset `id`; a blank name keeps the old one (spec S254).
    fn rename_preset(&mut self, id: &str, name: &str);
    /// Update to Current Settings: what plays now goes into preset `id`,
    /// which keeps its ID, name and place.
    fn update_preset(&mut self, id: &str) -> Result<(), PresetError>;
    /// Deletes preset `id`; nothing that plays changes.
    fn delete_preset(&mut self, id: &str);
    /// Recalls preset `id` (spec S252, S265) and posts or clears the preset
    /// notice.
    fn recall_preset(&mut self, id: &str);
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

    /// "Calibrate Zone…" in zone `id`'s Options menu (spec S255): Calibrate
    /// All over the zone, every other speaker that plays silenced for the
    /// run.
    fn start_calibrate_zone(&mut self, _id: &str) {}

    /// Cancel on the running Calibrate All or Measure Again: nothing
    /// changes.
    fn cancel_calibrate_all(&mut self) {}

    /// Measure Again on the results' row `row`.
    fn measure_again(&mut self, _row: usize) {}

    /// Include Anyway (`true`) or Leave Out on the Calibrate All card's
    /// row of output `id`, an endpoint ID.
    fn include_in_calibration(&mut self, _id: &str, _included: bool) {}

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

    /// While an input device is captured, the largest sample magnitude
    /// since the last call (1 is full scale), for the level meter; `None`
    /// for any other source and while stopped. The UI calls it every
    /// `METER_POLL_MS` while the meter shows.
    fn input_peak(&mut self) -> Option<f32> {
        None
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
    /// File > AirPlay 2 (Experimental), as saved; the sender reads it at
    /// launch only.
    pub airplay2_experimental: bool,
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

/// While a test holds it, the check of a host under `.held` waits.
#[cfg(test)]
pub static MOCK_CAST_HOLD: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A host under `.invalid` (RFC 6761) stands for one nothing answers.
#[cfg(any(test, feature = "mock-backend"))]
fn mock_cast_answers(address: &crate::audio::cast_address::CastAddress) -> bool {
    #[cfg(test)]
    if address.host().ends_with(".held") {
        drop(MOCK_CAST_HOLD.lock());
    }
    !address.host().ends_with(".invalid")
}

/// The subject of the example data's preset notice.
#[cfg(any(test, feature = "mock-backend"))]
const PRESETS: &str = "Presets";

/// Example data for UI work and tests: no device is opened, nothing plays.
#[cfg(any(test, feature = "mock-backend"))]
pub struct MockBackend {
    state: AppState,
    next_id: u64,
    view: ViewSettings,
    /// The rows of the hidden speakers, to list them again as they were.
    hidden_rows: Vec<Speaker>,
    presets: Vec<Preset>,
    /// The preset the preset notice is about.
    preset_notice: Option<String>,
}

#[cfg(any(test, feature = "mock-backend"))]
impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(test, feature = "mock-backend"))]
impl MockBackend {
    /// [`capture_preset`] with the example's members kept: their IDs are no
    /// speaker keys, so the core's clean-up would drop them all.
    fn capture_preset(&self, id: &str, name: &str) -> Result<Preset, PresetError> {
        let state = self.snapshot();
        let mut preset = capture_preset(id, name, &state).ok_or(PresetError::NoSpeakerOn)?;
        preset.members = speakers_on_now(&state)
            .into_iter()
            .map(|speaker| policy_presets::PresetMember {
                key: speaker.key,
                volume: speaker.volume,
            })
            .collect();
        Ok(preset)
    }

    pub fn new() -> Self {
        let source = |id: &str, name: &str, kind| Source {
            id: id.into(),
            name: name.into(),
            kind,
            channels: 0,
        };
        let speaker = |id: &str, name: &str, kind, enabled, delay_ms| Speaker {
            id: id.into(),
            name: name.into(),
            system_name: name.into(),
            kind,
            enabled,
            volume: volume::DEFAULT_LOCAL_POSITION,
            delay_ms,
            auto_connect: false,
            availability: Availability::Open,
            paired: false,
            holds_password: false,
            device_volume: None,
            waiting: None,
            cast: None,
            group: None,
            mic_left_out: None,
            in_target: None,
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
                Source {
                    channels: 5,
                    ..source("input:line", "Line In", SourceKind::Input)
                },
            ],
            source_id: "system".into(),
            input_pair: 0,
            mix_apps: vec![
                MixApp {
                    id: "player.exe".into(),
                    name: "Music Player".into(),
                    checked: false,
                },
                MixApp {
                    id: "browser.exe".into(),
                    name: "Web Browser".into(),
                    checked: false,
                },
            ],
            mix_all_except: false,
            master_volume: volume::DEFAULT_MASTER_POSITION,
            speakers,
            hidden: Vec::new(),
            zones: Vec::new(),
            absent: Vec::new(),
            equalizer: EqSettings::default(),
            equalizer_curves: Vec::new(),
            presets: Vec::new(),
            current_preset: None,
            notices: Vec::new(),
            pin_prompt: None,
            password_prompt: None,
            reference_name: Some("Speakers".into()),
            airplay_delay_ms: Some(1750.0),
            airplay_none_found: false,
            airplay_searching: false,
            when_source_quits: WhenSourceQuits::Stop,
            cast_list: CastList::Quiet,
            cast_by_address: true,
            calibration: CalibrationState::default(),
            calibrate_all: CalibrateAllState::default(),
        };
        MockBackend {
            state,
            next_id: 1,
            view: ViewSettings::default(),
            hidden_rows: Vec::new(),
            presets: Vec::new(),
            preset_notice: None,
        }
    }

    /// Posts a notice about `subject`, replacing an older one about it.
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

    /// Keeps only the example speakers `keep` accepts, and the zones.
    #[cfg(test)]
    pub fn retain_speakers(&mut self, keep: impl Fn(&Speaker) -> bool) {
        self.state.speakers.retain(keep);
    }

    /// Adds preset `id` of the speakers `members` at the master and the
    /// curve now, with no own volumes.
    #[cfg(test)]
    pub fn add_preset(&mut self, id: &str, name: &str, members: &[&str]) {
        self.presets.push(Preset {
            id: id.to_owned(),
            name: name.to_owned(),
            members: members
                .iter()
                .map(|key| policy_presets::PresetMember {
                    key: (*key).to_owned(),
                    volume: None,
                })
                .collect(),
            zones: Vec::new(),
            master: self.state.master_volume,
            gains_db: self.state.equalizer.gains_db,
        });
    }

    /// Adds a zone of the speakers `members`, which leave any other zone.
    #[cfg(test)]
    pub fn add_zone(&mut self, id: &str, name: &str, members: &[&str]) {
        self.zones_mut(|zones| add_zone(zones, id, name, members));
    }

    /// The example data with zones, for `AUDIOSYNC_PREVIEW_ZONES`: Living
    /// Room holds a waiting speaker, a stereo pair, a Google Cast speaker,
    /// a hidden one (not counted) and one not found yet; Upstairs a speaker
    /// that plays as part of the pair; Office is empty. Streaming is on, and
    /// the notice of groups that became zones shows.
    pub fn zones_preview() -> Self {
        let mut mock = MockBackend::new();
        let state = &mut mock.state;
        state.running = true;
        let template = state.speakers[2].clone();
        let pair = Speaker {
            id: "airplay:pair".into(),
            name: "Living Room Pair".into(),
            system_name: "Living Room Pair".into(),
            kind: SpeakerKind::AirPlay2,
            enabled: true,
            auto_connect: false,
            group: Some(crate::audio::backend::AirPlayGroup {
                kind: crate::audio::backend::AirPlayGroupKind::StereoPair,
                members: vec!["Left".into(), "Right".into()],
                missing: Vec::new(),
                incomplete: false,
            }),
            ..template.clone()
        };
        let left = Speaker {
            id: "airplay:left".into(),
            name: "Left".into(),
            system_name: "Left".into(),
            enabled: false,
            auto_connect: false,
            in_target: Some(InTarget {
                id: pair.id.clone(),
                name: pair.name.clone(),
            }),
            ..template.clone()
        };
        let attic = Speaker {
            id: "cc:attic".into(),
            name: "Attic".into(),
            system_name: "Attic".into(),
            kind: SpeakerKind::Cast,
            enabled: false,
            auto_connect: false,
            delay_ms: Some(0.0),
            cast: Some(CastRow {
                state: CastState::Idle,
                muted: false,
                volume_fixed: false,
                volume_step: 1.0,
                group: None,
                added_by_address: false,
            }),
            ..template
        };
        state.speakers.extend([pair, left, attic]);
        if let Some(kitchen) = state
            .speakers
            .iter_mut()
            .find(|s| s.id == "airplay:kitchen")
        {
            kitchen.waiting = Some(Waiting::Reconnecting);
        }
        state.hidden.push(HiddenSpeaker {
            id: "airplay:den".into(),
            name: "Den".into(),
            kind_text: "AirPlay".into(),
            present: None,
        });
        state.absent.push(AbsentMember {
            id: "airplay:theater:4C2D".into(),
            name: "Home Theater".into(),
            kind: SpeakerKind::AirPlay2,
        });
        state.notices.push(Notice {
            id: 1,
            subject: "Zones".into(),
            message: NoticeMessage::GroupsBecameZones,
            time: "09.40".into(),
        });
        mock.next_id = 2;
        mock.zones_mut(|zones| {
            add_zone(
                zones,
                "zone:living",
                "Living Room",
                &[
                    "airplay:kitchen",
                    "airplay:pair",
                    "cc:attic",
                    "local:desk",
                    "airplay:den",
                    "airplay:theater:4C2D",
                ],
            );
            add_zone(
                zones,
                "zone:upstairs",
                "Upstairs",
                &["airplay:bedroom", "airplay:left"],
            );
            add_zone(zones, "zone:office", "Office", &[]);
        });
        mock
    }

    fn speaker_mut(&mut self, id: &str) -> Option<&mut Speaker> {
        self.state.speakers.iter_mut().find(|s| s.id == id)
    }

    /// Changes the zones through the core's rules (S245).
    fn zones_mut<R>(&mut self, change: impl FnOnce(&mut Zones) -> R) -> R {
        let mut zones = Zones::from_stored(std::mem::take(&mut self.state.zones));
        let changed = change(&mut zones);
        self.state.zones = zones.into_list();
        changed
    }

    fn next_zone_id(&mut self) -> String {
        let id = format!("zone:{}", self.next_id);
        self.next_id += 1;
        id
    }
}

/// Adds zone `id` with the speakers `members`, which leave any other zone.
#[cfg(any(test, feature = "mock-backend"))]
fn add_zone(zones: &mut Zones, id: &str, name: &str, members: &[&str]) {
    zones.new_zone(id, name, |n| format!("Zone {n}"));
    for member in members {
        zones.move_speaker(member, id);
    }
}

#[cfg(any(test, feature = "mock-backend"))]
impl Backend for MockBackend {
    fn snapshot(&self) -> AppState {
        let mut state = self.state.clone();
        show_presets(&mut state, &self.presets);
        state
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
            self.state.input_pair = 0;
        }
    }

    fn select_input_pair(&mut self, pair: u16) {
        let chosen = self
            .state
            .sources
            .iter()
            .find(|source| source.id == self.state.source_id);
        if chosen.is_some_and(|source| source.kind == SourceKind::Input) {
            self.state.input_pair = pair;
        }
    }

    fn toggle_mix_app(&mut self, executable: &str) {
        if let Some(app) = self
            .state
            .mix_apps
            .iter_mut()
            .find(|app| app.id == executable)
        {
            app.checked = !app.checked;
        }
        let checked: Vec<&str> = self
            .state
            .mix_apps
            .iter()
            .filter(|app| app.checked)
            .map(|app| app.id.as_str())
            .collect();
        // Two apps make a mix, as the real backend's source key shows it.
        self.state.source_id = if checked.len() > 1 {
            format!("mix:only:{}", checked.join(","))
        } else {
            "system".into()
        };
    }

    fn set_mix_all_except(&mut self, on: bool) {
        self.state.mix_all_except = on;
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
        // Like the adapter: an unpaired speaker that asks for a code prompts for it.
        if let Some(speaker) = self.state.speaker(id).filter(|speaker| {
            on && speaker.availability == Availability::NeedsCode && !speaker.paired
        }) {
            self.state.pin_prompt = Some(PinPrompt {
                speaker_id: speaker.id.clone(),
                name: speaker.name.clone(),
                phase: PinPhase::Ready,
                wrong_pin: false,
            });
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

    fn rename_speaker(&mut self, id: &str, name: &str) {
        if let Some(speaker) = self.speaker_mut(id) {
            speaker.name = speaker_names::shown_after(name, &speaker.system_name);
        }
    }

    fn hide_speaker(&mut self, id: &str, kind: &str) {
        let Some(index) = self.state.speakers.iter().position(|s| s.id == id) else {
            return;
        };
        if self.state.speakers[index].is_reference() {
            return;
        }
        let mut row = self.state.speakers.remove(index);
        row.enabled = false;
        self.state.hidden.push(HiddenSpeaker {
            id: row.id.clone(),
            name: row.name.clone(),
            kind_text: kind.to_owned(),
            present: Some(row.kind),
        });
        self.hidden_rows.push(row);
    }

    fn unhide_speaker(&mut self, id: &str) {
        self.state.hidden.retain(|hidden| hidden.id != id);
        if let Some(index) = self.hidden_rows.iter().position(|s| s.id == id) {
            let row = self.hidden_rows.remove(index);
            self.state.speakers.push(row);
        }
    }

    fn toggle_zone(&mut self, id: &str) {
        self.toggle_zone_from_phone(id, &[]);
    }

    fn toggle_zone_from_phone(&mut self, id: &str, keep_off: &[String]) {
        let Some(zone) = self.state.zones.iter().find(|z| z.id == id).cloned() else {
            return;
        };
        let turn_on = self.state.zone_state(&zone).click_turns_on();
        let mut cast_on = false;
        for member in &zone.members {
            if keep_off.contains(member) {
                continue;
            }
            let Some(speaker) = self.speaker_mut(member) else {
                continue;
            };
            if speaker.in_target.is_some() {
                continue;
            }
            if turn_on && speaker.kind == SpeakerKind::Cast {
                // One Google Cast speaker plays at a time.
                speaker.enabled |= !cast_on;
                cast_on = true;
                continue;
            }
            speaker.enabled = turn_on;
        }
        if turn_on {
            self.state.running = true;
        }
    }

    fn new_zone(&mut self, name: &str) -> Result<(), ZoneError> {
        let members: Vec<String> = self
            .state
            .speakers
            .iter()
            .filter(|s| s.enabled)
            .map(|s| s.id.clone())
            .collect();
        if members.is_empty() {
            return Err(ZoneError::NoActiveSpeakers);
        }
        let id = self.next_zone_id();
        let members: Vec<&str> = members.iter().map(String::as_str).collect();
        self.zones_mut(|zones| add_zone(zones, &id, name, &members));
        Ok(())
    }

    fn new_zone_with(&mut self, speaker_id: &str, name: &str) {
        let id = self.next_zone_id();
        self.zones_mut(|zones| add_zone(zones, &id, name, &[speaker_id]));
    }

    fn rename_zone(&mut self, id: &str, name: &str) {
        self.zones_mut(|zones| zones.rename(id, name));
    }

    fn delete_zone(&mut self, id: &str) {
        self.zones_mut(|zones| zones.delete(id));
    }

    fn move_to_zone(&mut self, speaker_id: &str, zone_id: &str) {
        self.zones_mut(|zones| zones.move_speaker(speaker_id, zone_id));
    }

    fn remove_from_zone(&mut self, speaker_id: &str) {
        self.zones_mut(|zones| zones.remove_speaker(speaker_id));
    }

    fn set_zone_volume(&mut self, id: &str, volume: f64) {
        self.zones_mut(|zones| zones.set_volume(id, volume));
    }

    /// A zone run holds the outputs as soon as it starts.
    fn start_calibrate_zone(&mut self, id: &str) {
        if self.state.zones.iter().any(|zone| zone.id == id) {
            self.state.calibration.busy = true;
            self.state.calibrate_all.phase = AllPhase::Running(AllProgress::Preparing);
        }
    }

    fn use_target_instead(&mut self, speaker_id: &str) {
        let Some(target) = self
            .state
            .speaker(speaker_id)
            .and_then(|s| s.in_target.clone())
        else {
            return;
        };
        self.zones_mut(|zones| zones.rename_key(speaker_id, &target.id));
    }

    fn check_cast_address(&mut self, text: &str) -> Result<CastAddressCheck, CastAddressError> {
        let address = crate::audio::cast_address::CastAddress::parse(text)
            .ok_or(CastAddressError::Invalid)?;
        Ok(CastAddressCheck {
            address,
            answers: mock_cast_answers,
        })
    }

    fn add_checked_cast_address(
        &mut self,
        address: crate::audio::cast_address::CastAddress,
        answered: bool,
    ) -> Result<(), CastAddressError> {
        if !answered {
            return Err(CastAddressError::NotFound);
        }
        let id = format!("cc:{}", address.text());
        if self.state.speaker(&id).is_none() {
            self.state.speakers.push(Speaker {
                id,
                name: address.text(),
                system_name: address.text(),
                kind: SpeakerKind::Cast,
                enabled: false,
                volume: 100.0,
                delay_ms: Some(0.0),
                auto_connect: false,
                availability: Availability::Open,
                paired: false,
                holds_password: false,
                device_volume: None,
                waiting: None,
                cast: Some(CastRow {
                    state: CastState::Idle,
                    muted: false,
                    volume_fixed: false,
                    volume_step: 1.0,
                    group: None,
                    added_by_address: true,
                }),
                group: None,
                mic_left_out: None,
                in_target: None,
            });
        }
        Ok(())
    }

    fn remove_cast_address(&mut self, id: &str) {
        self.state.speakers.retain(|speaker| {
            speaker.id != id
                || !speaker
                    .cast
                    .as_ref()
                    .is_some_and(|cast| cast.added_by_address)
        });
    }

    fn set_equalizer(&mut self, settings: EqSettings) {
        self.state.equalizer = settings.quantized();
    }

    fn save_preset(&mut self, name: &str) -> Result<(), PresetError> {
        if self.presets.len() >= MAX_PRESETS {
            return Err(PresetError::Full);
        }
        let id = new_preset_id(&self.presets);
        let preset = self.capture_preset(&id, name)?;
        self.presets.push(preset);
        Ok(())
    }

    fn rename_preset(&mut self, id: &str, name: &str) {
        let Some(preset) = self.presets.iter_mut().find(|preset| preset.id == id) else {
            return;
        };
        preset.name = policy_presets::renamed_preset(&preset.name, name);
        let name = preset.name.clone();
        if self.preset_notice.as_deref() == Some(id)
            && let Some(notice) = self.state.notices.iter_mut().find(|n| n.subject == PRESETS)
            && let NoticeMessage::PresetLeftOff { preset, .. } = &mut notice.message
        {
            *preset = name;
        }
    }

    fn update_preset(&mut self, id: &str) -> Result<(), PresetError> {
        let Some(index) = self.presets.iter().position(|preset| preset.id == id) else {
            return Ok(());
        };
        let name = self.presets[index].name.clone();
        self.presets[index] = self.capture_preset(id, &name)?;
        Ok(())
    }

    fn delete_preset(&mut self, id: &str) {
        self.presets.retain(|preset| preset.id != id);
        if self.preset_notice.as_deref() == Some(id) {
            self.state.notices.retain(|n| n.subject != PRESETS);
            self.preset_notice = None;
        }
    }

    fn recall_preset(&mut self, id: &str) {
        self.recall_preset_from_phone(id, &[]);
    }

    fn recall_preset_from_phone(&mut self, id: &str, keep_off: &[String]) {
        let Some(preset) = self.presets.iter().find(|preset| preset.id == id).cloned() else {
            return;
        };
        let mut left_off = Vec::new();
        let mut members = Vec::new();
        // A phone's recall leaves the members that ask for a code off (S257).
        for member in preset.members.iter().filter(|m| !keep_off.contains(&m.key)) {
            if let Some(hidden) = self.state.hidden.iter().find(|h| h.id == member.key) {
                left_off.push((hidden.name.clone(), PresetLeftOff::Hidden));
            } else if self.state.speaker(&member.key).is_some() {
                members.push((member.key.clone(), member.volume));
            } else {
                left_off.push((member.key.clone(), PresetLeftOff::NotFound));
            }
        }
        let nothing_changed = members.is_empty();
        if !nothing_changed {
            for speaker in &mut self.state.speakers {
                if !speaker.is_reference() && !members.iter().any(|(key, _)| *key == speaker.id) {
                    speaker.enabled = false;
                }
            }
            // As its own switch does, so one that asks for a code asks.
            for (key, _) in &members {
                if self
                    .state
                    .speaker(key)
                    .is_some_and(|speaker| !speaker.enabled)
                {
                    self.set_speaker_enabled(key, true);
                }
            }
            for (key, volume) in members {
                if let (Some(speaker), Some(volume)) = (self.speaker_mut(&key), volume) {
                    speaker.volume = volume;
                }
            }
            self.zones_mut(|zones| {
                let volumes: Vec<(String, f64)> =
                    policy_presets::recall_zone_volumes(&preset, zones)
                        .into_iter()
                        .map(|(zone, volume)| (zone.to_owned(), volume))
                        .collect();
                for (zone, volume) in volumes {
                    zones.set_volume(&zone, volume);
                }
            });
            self.state.master_volume = preset.master;
            self.state.equalizer.gains_db = preset.gains_db;
            self.state.running = true;
        }
        match preset_notice(&preset.name, left_off, nothing_changed) {
            Some(message) => {
                self.post(PRESETS, message, "09.41");
                self.preset_notice = Some(preset.id);
            }
            None => {
                self.state.notices.retain(|n| n.subject != PRESETS);
                self.preset_notice = None;
            }
        }
    }

    fn set_equalizer_curves(&mut self, curves: Vec<UserCurve>) {
        self.state.equalizer_curves = eq::sanitized_curves(curves);
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

/// The curve the equalizer picker shows: the first preset, then the first
/// saved curve, with the current gains (spec S270).
pub fn shown_curve(state: &AppState) -> Option<CurveMatch> {
    eq::matching_curve(&state.equalizer.gains_db, &state.equalizer_curves)
}

/// `curve`'s entry in the picker, presets first; -1 for none ("Custom").
pub fn picker_index(curve: Option<CurveMatch>) -> i32 {
    let index = match curve {
        Some(CurveMatch::Preset(index)) => index,
        Some(CurveMatch::User(index)) => eq::PRESETS.len() + index,
        None => return -1,
    };
    i32::try_from(index).unwrap_or(-1)
}

/// The picker's entry `index` as a curve, presets first.
pub fn picked_curve(index: i32, saved: usize) -> Option<CurveMatch> {
    let index = usize::try_from(index).ok()?;
    match index.checked_sub(eq::PRESETS.len()) {
        None => Some(CurveMatch::Preset(index)),
        Some(user) if user < saved => Some(CurveMatch::User(user)),
        Some(_) => None,
    }
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
    fn the_live_input_note_shows_only_while_an_input_plays_through_airplay() {
        let mut state = MockBackend::new().snapshot();
        let airplay = |s: &Speaker| matches!(s.kind, SpeakerKind::AirPlay | SpeakerKind::AirPlay2);
        for speaker in &mut state.speakers {
            speaker.enabled = !airplay(speaker);
        }
        state.source_id = "input:line".to_owned();
        state.airplay_delay_ms = Some(1750.0);
        assert_eq!(
            state.live_input_airplay_wait(),
            None,
            "no AirPlay speaker is on"
        );

        let first = state
            .speakers
            .iter()
            .position(airplay)
            .expect("an AirPlay speaker");
        state.speakers[first].enabled = true;
        assert_eq!(
            state.live_input_airplay_wait(),
            Some(AirPlayWait::Tenths(18))
        );
        state.airplay_delay_ms = None;
        assert_eq!(state.live_input_airplay_wait(), Some(AirPlayWait::Unknown));

        for other in ["system", "app:player"] {
            state.source_id = other.to_owned();
            assert_eq!(state.live_input_airplay_wait(), None, "{other}");
        }
    }

    #[test]
    fn the_flyout_says_so_when_only_connected_speakers_are_hidden() {
        let mut backend = MockBackend::new();
        backend.add_zone("zone:1", "Zone 1", &[]);
        let connected: Vec<String> = backend
            .snapshot()
            .speakers
            .iter()
            .filter(|s| !s.kind.is_this_pc())
            .map(|s| s.id.clone())
            .collect();
        for id in &connected {
            backend.hide_speaker(id, "AirPlay");
        }
        let state = backend.snapshot();
        assert!(!state.connected_all_hidden(), "a zone is still listed");
        backend.delete_zone("zone:1");
        let state = backend.snapshot();
        assert!(state.connected_all_hidden());
        assert!(!state.all_hidden(), "the PC's own speakers are listed");

        backend.unhide_speaker(&connected[0]);
        assert!(!backend.snapshot().connected_all_hidden());
        let mut only_this_pc = MockBackend::new();
        only_this_pc.hide_speaker("local:headphones", "Headphones");
        assert!(!only_this_pc.snapshot().connected_all_hidden());
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
            system_name: "X".into(),
            kind,
            enabled,
            volume: 60.0,
            delay_ms: Some(0.0),
            auto_connect: false,
            availability: Availability::Open,
            paired: false,
            holds_password: false,
            device_volume: None,
            waiting: None,
            cast: None,
            group: None,
            mic_left_out: None,
            in_target: None,
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
    fn a_zone_is_on_when_all_its_present_members_are() {
        let mut backend = MockBackend::new();
        backend.add_zone(
            "zone:a",
            "Downstairs",
            &["airplay:kitchen", "airplay:living"],
        );
        let zone = backend.snapshot().zones[0].clone();
        assert_eq!(backend.snapshot().zone_state(&zone), SpeakerState::Off);
        backend.toggle_zone(&zone.id);
        let state = backend.snapshot();
        assert_eq!(state.zone_state(&zone), SpeakerState::Playing);
        backend.toggle_zone(&zone.id);
        let state = backend.snapshot();
        assert!(state.zone_members(&zone).all(|s| !s.enabled));
    }

    #[test]
    fn a_zone_waits_when_all_its_present_members_wait() {
        let mut backend = MockBackend::new();
        backend.add_zone(
            "zone:a",
            "Downstairs",
            &["airplay:kitchen", "airplay:living"],
        );
        let zone = backend.snapshot().zones[0].clone();
        backend.toggle_zone(&zone.id);
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
            state.zone_state(&zone),
            SpeakerState::Playing,
            "one member still plays"
        );
        wait(&mut state, "airplay:living", Some(Waiting::Reconnecting));
        assert_eq!(
            state.zone_state(&zone),
            SpeakerState::Waiting(Waiting::Reconnecting),
            "every present member waits"
        );
        state.speakers.retain(|s| s.id != "airplay:living");
        assert_eq!(
            state.zone_state(&zone),
            SpeakerState::Waiting(Waiting::Reconnecting),
            "a member that went doesn't count"
        );
        assert!(
            !state.zone_state(&zone).click_turns_on(),
            "a click turns a waiting zone off"
        );
        state.running = false;
        assert_eq!(state.zone_state(&zone), SpeakerState::WillPlay);
    }

    #[test]
    fn a_zone_counts_its_members_but_the_hidden_ones() {
        let state = MockBackend::zones_preview().snapshot();
        let living = &state.zones[0];
        assert_eq!(
            state.zone_counts(living),
            ZoneCounts {
                total: 5,
                present: 4,
                on: 2,
                waiting: 1
            },
            "Den is hidden; the home theater isn't found yet"
        );
        let office = &state.zones[2];
        assert_eq!(state.zone_counts(office), ZoneCounts::default());
    }

    // covers: S255
    #[test]
    fn calibrate_zone_is_offered_while_a_member_is_here_and_no_calibration_holds_the_outputs() {
        let mut state = MockBackend::zones_preview().snapshot();
        let (living, office) = (state.zones[0].clone(), state.zones[2].clone());
        assert!(state.offers_zone_run(&living));
        assert!(
            !state.offers_zone_run(&office),
            "none of its members is here"
        );
        state.calibration.busy = true;
        assert!(
            !state.offers_zone_run(&living),
            "a start would do nothing while a calibration holds the outputs"
        );
    }

    #[test]
    fn a_zone_with_a_cast_member_is_on_with_one_cast_speaker() {
        let mut backend = MockBackend::new();
        backend.add_speaker(Speaker {
            id: "cc:attic".into(),
            kind: SpeakerKind::Cast,
            enabled: false,
            ..backend.snapshot().speakers[3].clone()
        });
        backend.add_speaker(Speaker {
            id: "cc:porch".into(),
            kind: SpeakerKind::Cast,
            enabled: false,
            ..backend.snapshot().speakers[3].clone()
        });
        backend.add_zone(
            "zone:a",
            "Garden",
            &["airplay:bedroom", "cc:attic", "cc:porch"],
        );
        backend.toggle_zone("zone:a");
        let state = backend.snapshot();
        let on: Vec<&str> = state
            .speakers
            .iter()
            .filter(|s| s.enabled && state.zone_of(&s.id).is_some())
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(on, ["airplay:bedroom", "cc:attic"]);
        assert_eq!(state.zone_state(&state.zones[0]), SpeakerState::Playing);
    }

    #[test]
    fn a_new_zone_needs_a_speaker_that_is_on_and_takes_it_from_its_zone() {
        let mut backend = MockBackend::new();
        backend.add_zone("zone:a", "Desk", &["local:desk"]);
        for id in ["local:speakers", "airplay:kitchen"] {
            backend.set_speaker_enabled(id, false);
        }
        assert_eq!(backend.new_zone("Office"), Err(ZoneError::NoActiveSpeakers));
        backend.set_speaker_enabled("local:desk", true);
        assert_eq!(backend.new_zone(" Office "), Ok(()));
        let state = backend.snapshot();
        let zone = state.zones.last().cloned().unwrap();
        assert_eq!(zone.name, "Office");
        assert_eq!(zone.members, ["local:desk"]);
        assert!(state.zones[0].members.is_empty(), "the empty zone stays");
    }

    #[test]
    fn use_target_instead_puts_the_pair_in_its_members_place() {
        let mut backend = MockBackend::zones_preview();
        backend.use_target_instead("airplay:left");
        let state = backend.snapshot();
        assert_eq!(state.zones[1].members, ["airplay:bedroom"]);
        assert_eq!(
            state.zone_of("airplay:pair").map(|zone| zone.id.as_str()),
            Some("zone:living"),
            "the pair stays where it was"
        );
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
        assert_eq!(
            shown_curve(&backend.snapshot()),
            Some(CurveMatch::Preset(0))
        );
        let mut settings = EqSettings::default();
        settings.gains_db[0] = 3.3;
        backend.set_equalizer(settings);
        assert_eq!(backend.snapshot().equalizer.gains_db[0], 3.5);
        assert_eq!(shown_curve(&backend.snapshot()), None);
        assert_eq!(picker_index(None), -1, "Custom is not an entry");
    }

    #[test]
    fn the_picker_lists_the_presets_then_the_saved_curves() {
        let mut backend = MockBackend::new();
        let mut gains = [0.0; eq::BAND_COUNT];
        gains[3] = 2.5;
        backend.set_equalizer_curves(vec![
            UserCurve {
                name: "Desk".into(),
                gains_db: gains,
            },
            UserCurve {
                name: "desk".into(),
                gains_db: [1.0; eq::BAND_COUNT],
            },
        ]);
        let state = backend.snapshot();
        assert_eq!(state.equalizer_curves.len(), 1, "a name is kept once");
        backend.set_equalizer(EqSettings {
            enabled: true,
            preamp_db: -4.0,
            gains_db: gains,
        });
        let shown = shown_curve(&backend.snapshot());
        assert_eq!(shown, Some(CurveMatch::User(0)), "the preamp never counts");
        let index = picker_index(shown);
        assert_eq!(index, i32::try_from(eq::PRESETS.len()).unwrap());
        assert_eq!(picked_curve(index, 1), shown);
        assert_eq!(picked_curve(0, 1), Some(CurveMatch::Preset(0)));
        assert_eq!(picked_curve(index + 1, 1), None);
        assert_eq!(picked_curve(-1, 1), None);
    }
}
