//! The interface between the UI and the audio backend.
//!
//! The UI owns one [`AudioBackend`] and calls it from its own thread: it lists
//! sources, outputs and AirPlay speakers, starts and stops playback, changes
//! their settings, and calls [`AudioBackend::poll`] about twice a second to
//! learn about device changes and AirPlay speakers coming and going. Which speakers to turn on, and when playback should start or stop
//! because of them, stays the UI's decision (the session rules in
//! `audiosync_core::policy::session`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use audiosync_cast::audio::Codec;
use audiosync_core::eq::EqSettings;
use audiosync_core::policy::device_volume::DeviceVolume;
use audiosync_core::policy::source_quit::WhenSourceQuits;

use super::calibration::{CalibrationTap, Engine, Microphone, MicrophoneError};
use super::cast_address::CastAddress;
use super::outputs::OutputSettings;
use super::routing::DirectPath;

/// The zone volume, 0 to 100, of every speaker in a zone (spec S249), by
/// kind under the backend's own IDs: endpoint, AirPlay and Cast device IDs.
/// A speaker not listed is in no zone.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ZoneVolumes {
    pub outputs: BTreeMap<String, f64>,
    pub airplay: BTreeMap<String, f64>,
    pub cast: BTreeMap<String, f64>,
}

/// What AudioSync captures.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SourceId {
    /// Everything Windows plays, except AudioSync's own output.
    WholeSystem,
    /// One app: the process `root_pid` and every process it started (a
    /// browser's tab and GPU processes, for example).
    App {
        /// The process ID of the app's top process.
        root_pid: u32,
    },
    /// Several apps mixed: each top process in `root_pids` and every
    /// process it started, each tree in its own loopback. A tree that no
    /// longer runs is skipped, and an app with an audio session running one
    /// of `executables` joins while the source plays.
    Apps {
        /// The process IDs of the apps' top processes.
        root_pids: Vec<u32>,
        /// The apps' executable file names, to find instances that start
        /// later.
        executables: Vec<String>,
    },
    /// Every app with an audio session except those running one of
    /// `executables` (file names, such as `spotify.exe`), and never
    /// AudioSync: each in its own loopback, as [`SourceId::Apps`].
    AppsExcept {
        /// The executable file names left out.
        executables: Vec<String>,
    },
    /// An input device (a microphone or line in): an active capture
    /// endpoint, by its endpoint ID.
    Input {
        /// The endpoint ID, the same across restarts and reboots.
        endpoint_id: String,
    },
}

/// A capture source the user can pick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceInfo {
    /// What to pass to [`AudioBackend::start`].
    pub id: SourceId,
    /// The app's name, from its executable, or the name Windows shows for an
    /// input device. For [`SourceId::WholeSystem`] it is an English
    /// fallback: the UI shows its own, translated label.
    pub name: String,
    /// The app's executable file name, such as `spotify.exe`. It stays the
    /// same when the app restarts (its process ID doesn't), so the UI can
    /// remember a choice by it. Empty for the whole system and for an input
    /// device.
    pub executable: String,
    /// An input device's channels in the format Windows runs it at, which
    /// decide the channel pairs it offers; 0 for other sources and when
    /// Windows doesn't say.
    pub channels: u16,
}

/// What kind of device an output is, as Windows describes its endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OutputKind {
    /// The PC's own speakers or line out.
    BuiltIn,
    /// Headphones or a headset on the PC's own jack.
    Headphones,
    /// A Bluetooth device.
    Bluetooth,
    /// A USB device whose form Windows doesn't tell (a line out, or not
    /// known).
    Usb,
    /// USB headphones or a USB headset.
    UsbHeadphones,
    /// USB speakers.
    UsbSpeakers,
    /// A display or TV over HDMI.
    Hdmi,
    /// A display over DisplayPort.
    DisplayPort,
    /// A software device, such as a virtual cable.
    Virtual,
    /// Anything else (S/PDIF, a remote session), or not known.
    Other,
}

/// A connected output device (a render endpoint) and its settings.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputInfo {
    /// The endpoint ID, the same across restarts and reboots.
    pub id: String,
    /// The name Windows shows for it.
    pub name: String,
    /// What kind of device it is.
    pub kind: OutputKind,
    /// What Windows said about its device, which decides whether microphone
    /// calibration can hear it.
    pub traits: super::outputs::DeviceTraits,
    /// Whether it is Windows' default output. AudioSync plays to each output
    /// by ID, so this is for display only.
    pub is_default: bool,
    /// Whether it is the reference output, which defines time 0 and has no
    /// delay setting (spec S174).
    pub is_reference: bool,
    /// Whether the user turned it on.
    pub enabled: bool,
    /// Its offset relative to the reference in milliseconds, −2000 to
    /// +20 000 (S175); kept, but not used, while it is the reference.
    pub delay_ms: f64,
    /// The volume slider position, 0 to 100.
    pub volume: f64,
    /// Whether it plays right now.
    pub playing: bool,
    /// The device's own volume and mute in Windows (spec S192, S194): read
    /// while the output is on, and `None` while it is off. Each part is
    /// `None` when Windows can't tell.
    pub device_volume: Option<DeviceVolume>,
}

/// Whether an AirPlay speaker can play from AudioSync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AirPlayAvailability {
    /// It can.
    Open,
    /// It asks for a password: switching it on asks the user for it, which
    /// AudioSync keeps in memory until it quits.
    NeedsPassword,
    /// An Apple TV that asks for a PIN once: switching it on pairs it, and
    /// its credentials are kept from then on.
    NeedsPin,
    /// It needs another kind of pairing with a PIN, which AudioSync can't do
    /// yet.
    PairingNotYet,
    /// Only AirPlay 2 can play it; AudioSync speaks AirPlay 1 only so far.
    AirPlay2NotYet,
    /// It wants an encryption, codec, transport or format AudioSync can't
    /// send.
    Unsupported,
}

/// An AirPlay speaker on the network, and its settings.
#[derive(Clone, Debug, PartialEq)]
pub struct AirPlayInfo {
    /// Its ID: the device ID the speaker announces, the same across
    /// restarts.
    pub id: String,
    /// The name it announces.
    pub name: String,
    /// Whether it can play from AudioSync.
    pub availability: AirPlayAvailability,
    /// Whether the user turned it on.
    pub enabled: bool,
    /// Its own volume s, 0 to 100, which is saved (S290).
    pub volume: f64,
    /// Its device level D, 0 to 100, which its row shows rounded down (S290).
    pub device_level: f64,
    /// Its own delay d_s in milliseconds, 0 to 20 000.
    pub delay_ms: f64,
    /// The latency it reported in this run, R_s in ms (a group's largest
    /// member's), 0 when it reported nothing (S283).
    pub reported_latency_ms: f64,
    /// Whether it plays right now.
    pub playing: bool,
    /// Whether AudioSync keeps credentials from pairing with it, on
    /// AirPlay 1 or AirPlay 2.
    pub paired: bool,
    /// Whether AudioSync holds the password the user typed for it, until
    /// AudioSync quits.
    pub holds_password: bool,
    /// Whether AudioSync stores pairing credentials for it, also ones the
    /// speaker turned down (P154), which Forget Pairing drops.
    pub holds_credentials: bool,
    /// Whether a pairing with it is running.
    pub pairing: bool,
    /// It asks for a code every time it connects over AirPlay 2 (status
    /// flag bit 3, P61), so auto-connect leaves it off.
    pub code_every_time: bool,
    /// AudioSync reaches it over AirPlay 2, which happens only while the
    /// AirPlay 2 switch is on.
    pub airplay2: bool,
    /// Set for a stereo pair or home theater group, listed as one speaker
    /// while the AirPlay 2 switch is on (spec P176).
    pub group: Option<AirPlayGroup>,
}

/// What a stereo pair or home theater group row says about its members.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AirPlayGroup {
    pub kind: AirPlayGroupKind,
    /// Every member's name, in view or not.
    pub members: Vec<String>,
    /// The members out of view or playing nothing now (P181).
    pub missing: Vec<String>,
    /// A member is missing, or reports its partner unreachable, even when
    /// no name is known for it.
    pub incomplete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AirPlayGroupKind {
    StereoPair,
    HomeTheater,
}

/// The PIN prompt of a running pairing: the Apple TV shows a PIN for the
/// user to type, or will again after a wrong one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AirPlayPinPrompt {
    /// The speaker's ID.
    pub id: String,
    /// The name it announces.
    pub name: String,
    /// Where the pairing is.
    pub phase: PairingPhase,
    /// The Apple TV turned the last PIN down; it shows a PIN again.
    pub wrong_pin: bool,
}

/// The password prompt of a speaker the user switched on: it asks for a
/// password, or turned the one typed down.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AirPlayPasswordPrompt {
    /// The speaker's ID.
    pub id: String,
    /// The name it announces.
    pub name: String,
    /// The speaker turned the last password down.
    pub wrong: bool,
}

/// Where a pairing is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairingPhase {
    /// Asking the Apple TV to show a PIN.
    Starting,
    /// The Apple TV shows a PIN; waiting for the user to type it.
    WaitingForPin,
    /// The PIN went to the Apple TV; waiting for its answer.
    Checking,
}

/// What went wrong with an AirPlay speaker, for a notice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AirPlayProblem {
    /// It didn't answer; AudioSync keeps trying.
    NotResponding,
    /// The connection died or the speaker went silent; AudioSync keeps
    /// trying.
    ConnectionLost,
    /// Another sender seems to be playing to it; AudioSync keeps trying.
    Busy,
    /// It turned the stream down, or answered in a way AudioSync doesn't
    /// understand; AudioSync keeps trying.
    Refused,
    /// It asks for a password AudioSync doesn't have, or turned the one it
    /// had down. Switching it on asks for it.
    NeedsPassword,
    /// An Apple TV that needs pairing with a PIN first (or again, after it
    /// stopped accepting AudioSync's credentials). Switching it on pairs it.
    NeedsPin,
    /// As `NeedsPin` for a speaker that asked for no PIN or password: it
    /// turned the session down before any pairing step, so the notice names
    /// no code. Switching it on pairs it all the same.
    NotAccepted,
    /// The pairing with an Apple TV failed, so it is off.
    PairingFailed,
    /// It needs another kind of pairing, which AudioSync can't do yet.
    PairingNotYet,
    /// Only AirPlay 2 can play it.
    AirPlay2NotYet,
    /// It turned NTP timing down, and PTP timing isn't available.
    TimingUnavailable,
    /// Sixteen AirPlay 2 speakers play: it stays on and starts once one of
    /// them stops.
    TooManyAirPlay2Speakers,
    /// It takes only PTP timing, which runs for speakers on another
    /// network: it stays on and starts once those stop.
    PtpOtherNetwork,
    /// An Apple TV whose home theater group plays: its sound goes to those
    /// speakers, so it is off.
    PlaysThroughSpeakers,
    /// A paired Apple TV didn't prove it is the one that was paired, so it
    /// is off; its stored pairing stays (P56).
    ReceiverUnverified,
    /// A home theater group whose Apple TV plays by itself, so the group is
    /// off.
    AppleTvPlays,
    /// A member of a stereo pair or home theater group that plays as part
    /// of it, so it stays off by itself.
    PlaysInGroup,
    /// AudioSync can't play to it at all.
    Unsupported,
}

impl AirPlayProblem {
    /// The sender keeps it on and starts it once it lets it (S285).
    pub fn waits(self) -> bool {
        matches!(
            self,
            AirPlayProblem::TooManyAirPlay2Speakers | AirPlayProblem::PtpOtherNetwork
        )
    }

    /// Whether AudioSync keeps trying by itself; otherwise the speaker is
    /// turned off.
    pub fn retried(self) -> bool {
        matches!(
            self,
            AirPlayProblem::NotResponding
                | AirPlayProblem::ConnectionLost
                | AirPlayProblem::Busy
                | AirPlayProblem::Refused
        )
    }
}

impl AirPlayAvailability {
    /// Why a speaker that isn't open can't play, or `None` when it can.
    pub fn problem(self) -> Option<AirPlayProblem> {
        match self {
            AirPlayAvailability::Open => None,
            AirPlayAvailability::NeedsPassword => Some(AirPlayProblem::NeedsPassword),
            AirPlayAvailability::NeedsPin => Some(AirPlayProblem::NeedsPin),
            AirPlayAvailability::PairingNotYet => Some(AirPlayProblem::PairingNotYet),
            AirPlayAvailability::AirPlay2NotYet => Some(AirPlayProblem::AirPlay2NotYet),
            AirPlayAvailability::Unsupported => Some(AirPlayProblem::Unsupported),
        }
    }
}

/// What a Google Cast speaker that is on is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CastStatus {
    /// Off, or on while streaming is off.
    Idle,
    /// Connecting, starting the player on the device, or loading the
    /// stream.
    Connecting,
    /// The device plays the stream.
    Playing,
    /// The connection dropped and AudioSync reconnects.
    Reconnecting,
}

/// What went wrong with a Google Cast speaker, for a notice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CastProblem {
    /// Another Cast speaker is on: one plays at a time (spec C2).
    Busy,
    /// It didn't answer.
    NotResponding,
    /// It wouldn't start the player, or turned the stream down.
    Refused,
    /// It couldn't fetch the stream from this PC, as when a firewall blocks it.
    Blocked,
    /// Another app or device started playing on it.
    TakenOver,
    /// The connection was lost and couldn't be brought back.
    ConnectionLost,
    /// It left the list after discovery stopped, with no session to keep it.
    SearchStopped,
}

/// Whether Google Cast discovery runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CastDiscovery {
    /// No Cast sender runs.
    Off,
    Browsing,
    /// Discovery couldn't start or stopped, so no Cast speaker is listed.
    #[cfg_attr(
        all(not(os_backend), test),
        expect(
            dead_code,
            reason = "only the real Cast host, which no test starts, browses"
        )
    )]
    Failed,
}

/// A Google Cast speaker on the network, and its settings.
#[derive(Clone, Debug, PartialEq)]
pub struct CastInfo {
    /// Its ID: the device `id` it announces, in hex.
    pub id: String,
    /// The name it announces.
    pub name: String,
    /// Set for a Cast group, which plays as one speaker (spec C3).
    pub group: Option<CastGroup>,
    /// Set for a speaker the user added by address (spec C18): the address
    /// as the settings keep it.
    pub address: Option<String>,
    /// No record or address check vouches for it now (spec C17, C18).
    pub offline: bool,
    /// Whether the user turned it on.
    pub enabled: bool,
    /// The device's volume as it last reported it, 0 to 100.
    pub volume: f64,
    /// A status reported a level or the user set one; else `volume` is 100.
    pub volume_known: bool,
    pub muted: bool,
    /// The device's volume is locked (`controlType` `fixed`, spec C68).
    pub volume_fixed: bool,
    /// How far the volume keys move it, 0 to 100: the device's own step
    /// (spec C69), or 1 when it reports none.
    pub volume_step: f64,
    /// The user's measured offset in milliseconds, 0 to 20 000: how long
    /// the other outputs wait for this speaker while it is on (spec C77).
    pub offset_ms: f64,
    pub status: CastStatus,
    /// The format its session plays, once the session reports it.
    pub format: Option<Codec>,
    /// How long its feed waits for L while it is in a session, in ms (C77).
    pub hold_back_ms: f64,
}

/// A Cast group as its row shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CastGroup {
    pub members: CastMembers<CastMember>,
}

/// A stereo pair or home theater group that holds an AirPlay speaker: its
/// AirPlay ID (`pair:` or `theater:` and more) and its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InTarget {
    pub id: String,
    pub name: String,
}

/// What a Cast group's row knows of its members (spec C94).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CastMembers<T = String> {
    /// It is off, or its session hasn't played yet.
    #[default]
    NotReported,
    /// Its session plays and named none of them.
    Unknown,
    /// The ones its session named, by name.
    Known(Vec<T>),
}

/// A member a Cast group's session named: the listed speaker it is, when
/// one is, so a name the user gave it can show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CastMember {
    pub id: Option<String>,
    pub name: String,
}

/// Something the UI should know about, from [`AudioBackend::poll`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendEvent {
    /// Outputs were connected or disconnected, renamed, or the default output
    /// changed: list them again.
    OutputsChanged,
    /// An output's device went away while it played. The output stays on and
    /// plays again, at its current settings, once the device is back.
    OutputLost {
        /// The endpoint ID.
        id: String,
    },
    /// An output could not start, so it was turned off.
    OutputFailed {
        /// The endpoint ID.
        id: String,
        /// Why it could not start.
        error: BackendError,
    },
    /// An output could not start because another app holds its device in
    /// exclusive mode. It stays on, is tried again every few seconds, and plays
    /// once the device is free; [`BackendEvent::OutputFree`] tells then.
    OutputBusy {
        /// The endpoint ID.
        id: String,
    },
    /// An output reported busy no longer waits for its device: it plays
    /// now, it was switched off, streaming stopped, or its device went
    /// away.
    OutputFree {
        /// The endpoint ID.
        id: String,
    },
    /// A lost capture's input device is held by another app in exclusive
    /// mode (S333). Streaming stays on and the input is tried again every
    /// few seconds; [`BackendEvent::CaptureFree`] tells once it no longer
    /// waits.
    CaptureBusy,
    /// A lost capture's input device is no longer listed (S329). Streaming
    /// stays on and the capture opens again once the device is listed;
    /// [`BackendEvent::CaptureFree`] tells once it no longer waits.
    InputAway,
    /// A capture waiting for its input device follows that device to a new
    /// endpoint ID, as an output's settings do (S329): it is the source now,
    /// and its channel pair, which the UI keeps, goes with it.
    InputMoved {
        /// The endpoint ID the device left.
        from: String,
        /// The endpoint ID it is listed under now.
        to: String,
        /// The name Windows lists it under now.
        name: String,
    },
    /// A capture reported busy, or waiting for its input device, no longer
    /// waits for its input: it captures again, or its session ended.
    CaptureFree,
    /// An output's stream stopped taking frames again before it had played
    /// five seconds since the last time (S326). The output stays on, its
    /// device still listed, and is opened again every ten seconds;
    /// [`BackendEvent::OutputPlaysAgain`] tells once a stream plays.
    OutputStalled {
        /// The endpoint ID.
        id: String,
    },
    /// An output that ran away (S347) did so again within about 30 s of
    /// its reopen. It plays on; [`BackendEvent::OutputPlaysAgain`] tells
    /// once it is switched off or streaming stops.
    OutputStruggling {
        /// The endpoint ID.
        id: String,
    },
    /// An output whose device Windows still lists has refused to open for
    /// about thirty seconds (S331). The output stays on and is tried again
    /// every few seconds; [`BackendEvent::OutputPlaysAgain`] tells once it
    /// plays.
    OutputRefused {
        /// The endpoint ID.
        id: String,
    },
    /// An output reported stalled has played five seconds since it was
    /// opened again, or one reported refused plays, or either no longer
    /// waits: it was switched off, failed, or streaming stopped; or one
    /// reported struggling was switched off, or streaming stopped.
    OutputPlaysAgain {
        /// The endpoint ID.
        id: String,
    },
    /// Playback stopped on its own.
    Stopped {
        /// Why it stopped.
        reason: StopReason,
    },
    /// The app being captured quit and the setting says to go on: the
    /// session now captures the whole system, every output still on and
    /// every AirPlay speaker still in the stream (spec S227).
    SwitchedToSystem,
    /// A source or channel pair started while streaming couldn't start, so
    /// the one the last session played plays again, to the same speakers
    /// (spec S141).
    SwitchFailed {
        /// Why the new one couldn't start.
        error: BackendError,
    },
    /// Whether the source is heard twice: straight from Windows, undelayed,
    /// as well as through AudioSync's delayed copies. Checked every few
    /// seconds while streaming and reported when the answer changes.
    HeardTwice {
        /// The endpoints the source is heard from straight, by endpoint ID;
        /// empty once it is heard only through AudioSync again, or streaming
        /// stopped.
        paths: Vec<DirectPath>,
    },
    /// AirPlay speakers appeared, changed, went away, or started or stopped
    /// playing: list them again.
    AirPlayChanged,
    /// An AirPlay speaker plays (again).
    AirPlayPlaying {
        /// The speaker's ID.
        id: String,
    },
    /// An AirPlay speaker stopped playing, and AudioSync keeps trying to
    /// reach it. It stays on.
    AirPlayTrouble {
        /// The speaker's ID.
        id: String,
        /// What went wrong.
        problem: AirPlayProblem,
    },
    /// An AirPlay speaker stopped announcing itself on the network, or was
    /// announced again (`unannounced` false) without playing yet. It stays
    /// on, and the sender starts it once it is announced (P215).
    AirPlayUnannounced {
        /// The speaker's ID.
        id: String,
        /// Whether it is out of view now.
        unannounced: bool,
    },
    /// An AirPlay speaker that waited for an AirPlay 2 place or for PTP was
    /// started by the sender again (S285).
    AirPlayWaitOver {
        /// The speaker's ID.
        id: String,
    },
    /// An AirPlay speaker can't play from AudioSync, so it was turned off.
    AirPlayFailed {
        /// The speaker's ID.
        id: String,
        /// Why.
        problem: AirPlayProblem,
    },
    /// A member of a stereo pair or home theater group asks for a password
    /// no prompt can take now, so the target was turned off.
    AirPlayMemberNeedsPassword {
        /// The target's ID.
        id: String,
        /// The member's name, which the notice gives: a member that is part
        /// of its target has no row of its own.
        member: String,
    },
    /// An Apple TV the user switched on was paired, and is on now.
    AirPlayPaired {
        /// The speaker's ID.
        id: String,
    },
    /// An output's own volume or mute changed in Windows (spec S195): show
    /// it again.
    DeviceVolumeChanged {
        /// The endpoint ID.
        id: String,
    },
    /// The AirPlay sender stopped after an internal error or couldn't
    /// start, so no AirPlay speaker is listed or plays, or its discovery
    /// did, so none is listed; [`AudioBackend::retry_airplay`] starts a new
    /// sender.
    AirPlayStopped,
    /// Another app holds PTP port 319 or 320, so AirPlay speakers that need
    /// PTP timing can't play (P158); reported once per sender.
    AirPlayPtpPortsTaken,
    /// A session bound the PTP ports after
    /// [`BackendEvent::AirPlayPtpPortsTaken`]: the ports are free again.
    AirPlayPtpFree,
    /// Google Cast speakers appeared, changed or went away, or one's state
    /// changed: list them again.
    CastChanged,
    /// A browse found the device a Cast speaker added by address is (S341):
    /// what is kept under the ID `from` moves to the device's ID `to`.
    CastIdentified {
        /// The ID the speaker had as an address.
        from: String,
        /// The device's own ID.
        to: String,
    },
    /// A Google Cast speaker plays.
    CastPlaying {
        /// The speaker's ID.
        id: String,
    },
    /// A Google Cast speaker couldn't play, or stopped, so it was turned
    /// off.
    CastFailed {
        /// The speaker's ID.
        id: String,
        /// Why.
        problem: CastProblem,
    },
    /// What AudioSync keeps per Cast device (offsets, formats) changed:
    /// save it.
    CastMemoryChanged,
    /// Whether the pairings' file could be written, reported when that
    /// changes: `false` once a write failed (the pairings added or forgotten
    /// since last only until AudioSync quits), `true` once one worked again.
    PairingStore {
        /// The last write worked.
        saved: bool,
    },
    /// The pairings' file the store ignored at launch (it couldn't be read,
    /// decrypted or parsed) was kept aside before the first write over it,
    /// so the user can be told where. Reported once.
    PairingsKeptAside {
        /// The copy's file name, in the app's data folder.
        file: String,
    },
}

/// Why playback stopped on its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// The app being captured quit.
    SourceQuit,
    /// Capture failed and could not go on.
    CaptureFailed(BackendError),
}

/// The HRESULT of a wait that timed out, `HRESULT_FROM_WIN32(ERROR_TIMEOUT)`:
/// the code of a [`BackendError::System`] for an activation Windows never
/// completed within AudioSync's wait.
pub const TIMED_OUT: i32 = 0x8007_05B4_u32 as i32;

/// Why something failed. The UI words each variant in its own, translated
/// text; [`fmt::Display`] gives English for logs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendError {
    /// The chosen app is AudioSync itself, or a process AudioSync started.
    OwnProcess,
    /// AudioSync runs inside the chosen app's process tree (that app started
    /// it), or may where process creation times can't tell, so capturing the
    /// tree could capture AudioSync's own output too.
    ContainsAudioSync,
    /// The chosen app is no longer running.
    SourceGone,
    /// The chosen input device isn't connected (unplugged, disabled or
    /// removed). Unlike [`BackendError::DeviceGone`], waiting doesn't help.
    InputGone,
    /// Windows denies AudioSync the input device (`E_ACCESSDENIED`): its
    /// privacy settings block microphone access for desktop apps.
    MicrophoneBlocked,
    /// The chosen process is not an app (the System or Idle process).
    NotAnApp,
    /// The output device is not connected, Windows audio is not running, or
    /// Windows is rebuilding the device's endpoint (after a wake, or a change
    /// of its format): worth another try.
    DeviceGone,
    /// Another app holds the device for itself (exclusive mode), so nothing
    /// else can play to it until that app lets go.
    DeviceBusy,
    /// The audio format can't be used, such as a rate outside 8 to 384 kHz.
    UnsupportedFormat,
    /// Windows audio didn't answer in time: opening a device hung (after a
    /// wake, say). Worth another try a little later.
    NotResponding,
    /// The system's audio service (PipeWire on Linux) isn't running, so
    /// nothing can be captured until the user starts it: not retried.
    #[cfg_attr(
        all(windows, not(test)),
        expect(dead_code, reason = "only the Linux backend reports it")
    )]
    ServiceNotRunning,
    /// This system's backend doesn't offer that yet, such as one app's sound
    /// or a local output on Linux.
    #[cfg_attr(
        all(windows, not(test)),
        expect(dead_code, reason = "only the Linux backend reports it")
    )]
    NotOffered,
    /// A Windows call failed with this HRESULT.
    System {
        /// The call that failed.
        call: &'static str,
        /// Its HRESULT.
        code: i32,
    },
}

/// A sentence in English for logs and diagnostics. The UI words errors in
/// its own, translated text; this sentence reaches the user only as the
/// detail under that text when Windows audio can't be opened at startup.
impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::OwnProcess => f.write_str("AudioSync can't capture its own sound."),
            BackendError::ContainsAudioSync => f.write_str(
                "AudioSync may run inside this app's process tree, so capturing the app \
                 could capture AudioSync too.",
            ),
            BackendError::SourceGone => f.write_str("The app isn't running anymore."),
            BackendError::InputGone => f.write_str("The input device isn't connected."),
            BackendError::MicrophoneBlocked => f.write_str(
                "Windows blocks microphone access for desktop apps in its privacy settings.",
            ),
            BackendError::NotAnApp => f.write_str("That process isn't an app."),
            BackendError::DeviceGone => {
                f.write_str("The device isn't connected, or Windows audio isn't running.")
            }
            BackendError::DeviceBusy => f.write_str("Another app is using the device exclusively."),
            BackendError::UnsupportedFormat => {
                f.write_str("The device's audio format isn't supported.")
            }
            BackendError::NotResponding => f.write_str("Windows audio didn't answer in time."),
            BackendError::ServiceNotRunning => f.write_str("The audio service isn't running."),
            BackendError::NotOffered => f.write_str("AudioSync can't do that on this system yet."),
            BackendError::System { call, code } => {
                // `{:08X}` of a negative i32 prints its two's complement, the
                // way Windows writes HRESULTs.
                write!(f, "{call} failed (0x{code:08X}).")
            }
        }
    }
}

impl std::error::Error for BackendError {}

/// The audio backend, as the UI sees it.
pub trait AudioBackend {
    /// The sources to offer: the whole system first, then each app that has
    /// opened an audio stream, by name, then each active input device, by
    /// name. AudioSync's own processes, and any app whose capture would
    /// include them, are left out.
    fn list_sources(&mut self) -> Result<Vec<SourceInfo>, BackendError>;

    /// The app that runs `executable` now, whether or not it has opened an
    /// audio stream: the app of process `root_pid` while that process still
    /// runs `executable`, else another app that runs it, or `None` when none
    /// does. The source list offers only apps with an audio stream, but a
    /// capture can start before its app plays (just opened, or paused).
    fn find_app(
        &mut self,
        root_pid: u32,
        executable: &str,
    ) -> Result<Option<SourceInfo>, BackendError>;

    /// Lists the connected outputs again, and returns them by name, with
    /// their settings. A failed listing keeps the last list.
    fn list_outputs(&mut self) -> Result<Vec<OutputInfo>, BackendError>;

    /// The connected outputs as last listed, by name, with their current
    /// settings. Unlike [`AudioBackend::list_outputs`], it doesn't ask the
    /// system again, so it is cheap enough for every refresh of the UI.
    fn outputs(&self) -> Vec<OutputInfo>;

    /// The IDs of the outputs turned on, connected or not, sorted.
    fn enabled_outputs(&self) -> Vec<String>;

    /// Starts capturing `source` and playing it on every enabled, connected
    /// output. A running session is stopped first, its AirPlay and Google
    /// Cast speakers staying in the stream; when `source` can't start, the
    /// source that session played starts again and
    /// [`BackendEvent::SwitchFailed`] tells (spec S141).
    fn start(&mut self, source: &SourceId) -> Result<(), BackendError>;

    /// Whether a source switch still waits for the last session's outputs
    /// to fade out: until it starts, the source before it is the one a
    /// failed switch goes back to.
    fn switch_waits(&self) -> bool;

    /// Stops capture and every output. Does nothing when nothing runs.
    fn stop(&mut self);

    /// The source being captured, or `None` when stopped.
    fn running_source(&self) -> Option<SourceId>;

    /// Whether the running session waits for its input device to be listed
    /// again (S329).
    fn waits_for_input(&self) -> bool;

    /// While an input device is captured, the largest sample magnitude
    /// captured since the last call (1 is full scale), for the level meter;
    /// `None` for any other source and while stopped.
    fn input_peak(&mut self) -> Option<f32> {
        None
    }

    /// Turns an output on or off. While playing, an output turned on starts
    /// at once if it is connected; if it can't start, it is turned off again
    /// and the error returned.
    fn set_output_enabled(&mut self, id: &str, enabled: bool) -> Result<(), BackendError>;

    /// Sets an output's offset relative to the reference in milliseconds,
    /// live: every playing output whose hold-back changes is re-timed. A
    /// value that isn't finite is ignored; others are clamped to −2000 to
    /// +20 000.
    fn set_output_delay(&mut self, id: &str, delay_ms: f64);

    /// Sets an output's volume slider position (0 to 100, on a cubic curve),
    /// live. A value that isn't finite is ignored.
    fn set_output_volume(&mut self, id: &str, position: f64);

    /// Whether an output has settings of its own, connected or not: it was
    /// turned on or off, or its delay or volume set, in this run or a saved
    /// one.
    fn has_output_settings(&self, id: &str) -> bool;

    /// Every output that has settings of its own, connected or not, with
    /// them, so an unplugged output's offset is saved too (S175).
    fn kept_output_settings(&self) -> Vec<(String, OutputSettings)>;

    /// Gives output `to` the settings of output `from` (on or off, offset,
    /// volume) and forgets `from`'s, for a device listed under a new
    /// endpoint ID. While playing, `to` starts at once if it is on and
    /// connected; if it can't start, it is turned off again and the error
    /// returned.
    fn move_output_settings(&mut self, from: &str, to: &str) -> Result<(), BackendError>;

    /// The fix under an output whose own volume in Windows is low or muted
    /// (spec S197): sets that volume to 75 % when it is low and unmutes it
    /// when it is muted, once each. Nothing else ever writes a device volume.
    /// The output shows the result once Windows reports the change.
    fn fix_device_volume(&mut self, id: &str);

    /// Sets the master volume slider position (0 to 100), which scales every
    /// output and AirPlay speaker, live. A value that isn't finite is
    /// ignored.
    fn set_master_volume(&mut self, position: f64);

    /// Sets every zone volume (spec S249): playing members get their new
    /// gain or level at once, as a master change gives them, and a member
    /// started later starts at it.
    fn set_zone_volumes(&mut self, zones: ZoneVolumes);

    /// The master volume slider position.
    fn master_volume(&self) -> f64;

    /// Sets the equalizer, live: it changes the captured audio once, before
    /// the ring, so every output hears the same equalized sound. The
    /// settings are kept for the next start.
    fn set_equalizer(&mut self, settings: EqSettings);

    /// The AirPlay speakers on the network, by name, with their settings;
    /// empty while there is no AirPlay sender.
    fn airplay_speakers(&self) -> Vec<AirPlayInfo>;

    /// Whether an AirPlay sender runs and browses for receivers: false
    /// without a sender, and once one stopped or its discovery did or
    /// couldn't start (reported as [`BackendEvent::AirPlayStopped`]).
    fn airplay_browsing(&self) -> bool;

    /// The IDs of the AirPlay speakers turned on, listed or not, sorted.
    fn enabled_airplay(&self) -> Vec<String>;

    /// Turns an AirPlay speaker on or off. While playing, a speaker turned on
    /// starts at once. One that can't play from AudioSync stays off, with the
    /// reason returned: [`AirPlayProblem::NeedsPin`] for an Apple TV to pair
    /// first ([`AudioBackend::start_airplay_pairing`]). Turning a speaker off
    /// also stops its pairing.
    fn set_airplay_enabled(&mut self, id: &str, enabled: bool) -> Result<(), AirPlayProblem>;

    /// Starts pairing with an Apple TV that asks for a PIN: it shows one, and
    /// [`AudioBackend::airplay_pin_prompt`] asks the UI for it. Paired, the
    /// speaker is turned on ([`BackendEvent::AirPlayPaired`]); a failure
    /// leaves it off with [`BackendEvent::AirPlayFailed`].
    fn start_airplay_pairing(&mut self, id: &str) -> Result<(), AirPlayProblem>;

    /// The PIN the user typed for the speaker's pairing. Returns whether it
    /// went to the Apple TV: only digits, while the pairing waits for one.
    fn submit_airplay_pin(&mut self, id: &str, pin: &str) -> bool;

    /// Stops the speaker's pairing; the speaker stays off.
    fn cancel_airplay_pairing(&mut self, id: &str);

    /// Forgets the speaker's pairing credentials: it is turned off, and asks
    /// for a PIN again the next time it is switched on.
    fn forget_airplay_pairing(&mut self, id: &str);

    /// The PIN prompt to show, if a pairing has one.
    fn airplay_pin_prompt(&self) -> Option<AirPlayPinPrompt>;

    /// The user switched the speaker on themselves and it is on: until it
    /// plays, a session that asks for a password, or turns the one
    /// AudioSync has down, asks the user for it instead of turning the
    /// speaker off. Auto-connect and restored switches never call this.
    fn begin_airplay_attempt(&mut self, id: &str);

    /// Asks the user for the password of a speaker they switched on that
    /// returned [`AirPlayProblem::NeedsPassword`]:
    /// [`AudioBackend::airplay_password_prompt`] shows it.
    fn ask_airplay_password(&mut self, id: &str) -> Result<(), AirPlayProblem>;

    /// The password the user typed for the speaker: kept in memory until
    /// AudioSync quits, never logged, and the speaker is turned on. Returns
    /// whether it was taken: not empty, while its prompt is open.
    fn submit_airplay_password(&mut self, id: &str, password: &str) -> bool;

    /// The user cancelled the speaker's password prompt: it is off.
    fn cancel_airplay_password(&mut self, id: &str);

    /// The password prompt to show, if one waits and no PIN prompt is open.
    fn airplay_password_prompt(&self) -> Option<AirPlayPasswordPrompt>;

    /// The AirPlay speakers whose pairing runs or whose password prompt
    /// waits, shown or queued, with the target a member's prompt switches on.
    fn airplay_waiting_for_user(&self) -> BTreeSet<String>;

    /// The stereo pair or home theater group that holds the AirPlay speaker
    /// `id` (see [`AirPlayProblem::PlaysInGroup`]).
    fn airplay_holding_target(&self, _id: &str) -> Option<InTarget> {
        None
    }

    /// The name the AirPlay speaker `id` was last listed under, also when
    /// its stereo pair or home theater holds it and it has no row.
    fn airplay_listed_name(&self, _id: &str) -> Option<String> {
        None
    }

    /// Sets an AirPlay speaker's own volume s (0 to 100), live: a saved
    /// speaker and a preset's member (S290). A value that isn't finite is
    /// ignored.
    fn set_airplay_volume(&mut self, id: &str, position: f64);

    /// An AirPlay speaker's row moved to `row`, its device level (S290).
    fn move_airplay_row(&mut self, id: &str, row: f64);

    /// The AirPlay speaker's volume slider is held (true) or let go
    /// (false): a level it reads back meanwhile is ignored (S271).
    fn set_airplay_volume_dragging(&mut self, _id: &str, _dragging: bool) {}

    /// A phone's volume step holds the speaker's row as a drag does, until
    /// its steps rest (S291).
    fn hold_airplay_volume_for_phone(&mut self, _id: &str) {}

    /// The now-playing card's play-pause went to `app`, shown playing or
    /// paused: the AirPlay speakers pause with it when the reading agrees
    /// (P213).
    fn airplay_play_pause_pressed(&mut self, _app: &str, _playing: bool) {}

    /// Sets an AirPlay speaker's own delay d_s in milliseconds, live. A value
    /// that isn't finite is ignored; others are clamped to 0 to 20 000. Tests
    /// only: the app never sets one (spec S180).
    #[cfg(test)]
    fn set_airplay_delay(&mut self, id: &str, delay_ms: f64);

    /// The AirPlay sender's base latency L_s in milliseconds, or `None`
    /// while there is no sender.
    fn airplay_sender_latency_ms(&self) -> Option<f64>;

    /// The L_s the AirPlay delay field's value is turned into A against,
    /// also while there is no sender (spec S178).
    fn airplay_adjustment_base_ms(&self) -> f64;

    /// The user's AirPlay delay adjustment A in milliseconds: L = L_s + A
    /// while an AirPlay speaker is chosen (spec S178).
    fn airplay_adjustment(&self) -> f64;

    /// Sets the AirPlay delay adjustment A, live: local outputs are re-timed
    /// while an AirPlay speaker is chosen. A value that isn't finite is
    /// ignored; others are clamped to −2000 to +20 000.
    fn set_airplay_adjustment(&mut self, adjustment_ms: f64);

    /// Replaces an AirPlay sender that stopped (see
    /// [`BackendEvent::AirPlayStopped`]) with a new one. The speakers that
    /// were on stay on, with their settings, and play again once the new
    /// sender lists them. A new sender that can't start reports
    /// `AirPlayStopped` again.
    fn retry_airplay(&mut self);

    /// The Google Cast speakers on the network, by name, with their
    /// settings; empty while there is no Cast sender.
    fn cast_speakers(&self) -> Vec<CastInfo> {
        Vec::new()
    }

    fn cast_discovery(&self) -> CastDiscovery {
        CastDiscovery::Off
    }

    /// A Cast speaker is on, listed or not: turning on another is refused.
    fn cast_on(&self) -> bool {
        false
    }

    /// The ID of the Cast speaker that is on, listed or not.
    fn cast_enabled(&self) -> Option<String> {
        None
    }

    /// Turns a Cast speaker on or off. One plays at a time: turning one on
    /// while another is on is refused with [`CastProblem::Busy`] (spec C2).
    /// While streaming, one turned on connects at once.
    fn set_cast_enabled(&mut self, _id: &str, _enabled: bool) -> Result<(), CastProblem> {
        Err(CastProblem::NotResponding)
    }

    /// Turns the Cast speaker saved as on at the last quit on once it is
    /// listed, unless one is switched on first (spec C112); false without a
    /// Cast sender.
    fn turn_cast_on_when_listed(&mut self, _id: &str) -> bool {
        false
    }

    /// The Cast speaker to save as on: the one on, else the one waiting to
    /// be listed (spec C112).
    fn cast_on_to_save(&self) -> Option<String> {
        None
    }

    /// Sets a Cast speaker's device volume, 0 to 100.
    fn set_cast_volume(&mut self, _id: &str, _position: f64) {}

    fn set_cast_muted(&mut self, _id: &str, _muted: bool) {}

    /// Sets a Cast speaker's measured offset in milliseconds, clamped to 0
    /// to 20 000: while it is on, the other outputs wait that long for it
    /// (spec C77). A value that isn't finite is ignored.
    fn set_cast_offset(&mut self, _id: &str, _offset_ms: f64) {}

    /// How far ahead of the Cast speaker that is on the AirPlay speakers
    /// play, in ms, because their delay can't grow past the sender's limit
    /// (spec C77, P99); 0 while they keep up.
    fn airplay_ahead_ms(&self) -> f64 {
        0.0
    }

    /// The user's Resync: the playing Cast speaker loads the stream again,
    /// back at its starting latency, after a short gap (spec C79).
    fn resync_cast(&mut self, _id: &str) {}

    /// Lists a Cast speaker at `address` for a network where discovery
    /// can't find it (spec C18); a host name is looked up off this thread.
    fn add_cast_address(&mut self, _address: CastAddress) {}

    /// Lists the speaker added at `address` as device `id`, the one a browse
    /// found there before a relaunch (spec C18).
    fn set_cast_found(&mut self, _address: &CastAddress, _id: &str) {}

    /// Takes back a speaker added by address, turning it off first.
    fn remove_cast_address(&mut self, _address: &CastAddress) {}

    /// What AudioSync keeps per Cast device, as JSON to save; `None` while
    /// there is no Cast sender.
    fn cast_memory_json(&self) -> Option<String> {
        None
    }

    /// Takes the entries of `json`, a Cast speakers' file, for each device
    /// field the Cast memory holds none of.
    fn fill_cast_memory(&mut self, _json: &str) {}

    /// What a session does when the app it captures quits (spec S227).
    fn set_when_source_quits(&mut self, setting: WhenSourceQuits);

    /// The channel pair input device `endpoint_id` plays from its next
    /// start (`InputPair::index`, 0 for channels 1-2). A running capture
    /// keeps its pair until started again.
    fn set_input_pair(&mut self, endpoint_id: &str, pair: u16);

    /// What a microphone calibration depends on: the session, the outputs
    /// that play, the reference and their hold-backs.
    fn calibration_engine(&self) -> Engine;

    /// The capture's calibration tap.
    fn calibration_tap(&self) -> Option<Arc<CalibrationTap>>;

    /// Only local output `id` is heard, the others at gain 0; `None`: every
    /// output at its own gain again.
    fn set_calibration_solo(&mut self, id: Option<&str>);

    /// Only AirPlay speaker `id` is heard, the others muted; `None`: every
    /// AirPlay speaker at its own volume again.
    fn set_calibration_airplay_solo(&mut self, _id: Option<&str>) {}

    /// Starts recording the default input device, with room for `seconds`.
    fn open_microphone(&mut self, seconds: f64) -> Result<Box<dyn Microphone>, MicrophoneError>;

    /// Handles device changes, lost outputs, the end of capture and what the
    /// AirPlay sender reports, and returns what the UI should know. Call it
    /// about twice a second.
    fn poll(&mut self) -> Vec<BackendEvent>;

    /// Quit's teardown: the outputs fade out, the AirPlay and Cast senders
    /// stop. A later call, or the drop, does nothing more.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn shut_down(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_call_shows_its_hresult_the_windows_way() {
        let error = BackendError::System {
            call: "IAudioClient::Initialize",
            code: 0x8889_0008_u32 as i32,
        };
        assert_eq!(
            error.to_string(),
            "IAudioClient::Initialize failed (0x88890008)."
        );
    }

    #[test]
    fn every_error_reads_as_a_sentence_in_the_log() {
        let errors = [
            BackendError::OwnProcess,
            BackendError::ContainsAudioSync,
            BackendError::SourceGone,
            BackendError::InputGone,
            BackendError::MicrophoneBlocked,
            BackendError::NotAnApp,
            BackendError::DeviceGone,
            BackendError::DeviceBusy,
            BackendError::UnsupportedFormat,
            BackendError::NotResponding,
            BackendError::ServiceNotRunning,
            BackendError::NotOffered,
            BackendError::System { call: "x", code: 1 },
        ];
        for error in errors {
            let text = error.to_string();
            assert!(text.ends_with('.'), "{text:?} is a sentence");
        }
    }
}
